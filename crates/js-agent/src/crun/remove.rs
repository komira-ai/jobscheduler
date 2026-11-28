//! The removal of a lease, on the worker or off it.
//!
//! A removal ends the container's supervisor if it still runs, deletes the container
//! (`crun delete`), unmounts its file system, removes its cgroup, its lease directory (the
//! writable layers) and its temporary directory, its pins and last its sidecar, which is
//! synced: work that waits on the disk and on other processes. [`Runtime::removing`]
//! runs it on a thread of the removal pool ([`THREADS`] threads, `js-remove`), so the
//! lifecycle's worker goes on with every other job, a start among them, while a finished
//! job's lease is taken apart.
//!
//! The thread is given the [`Run`], taken out of the runtime's books, and a [`Remover`]:
//! shares of the image store, the ledger and the mounts' calls, nothing of the worker's. A
//! removal that succeeded answers nothing more; one that failed answers the run, which the
//! worker puts back in its books when it next asks ([`CrunRuntime::remove_off_worker`]), so a
//! later removal goes over it again from the start, as a failed [`Runtime::remove`] always
//! did. While a removal is under way its lease's name is refused to a create and to a
//! [`Runtime::remove`]: the thread holds the lease.
//!
//! A removal has no deadline of its own: its waits are bounded where they are made (the end
//! of the run after its cgroup was killed, by the kill wait, then the end of a supervisor
//! ended by force, by the force wait). A call that hangs in the kernel (a disk, or a file
//! system that never answers an unmount) holds its thread and its job, which stays in its
//! cleanup with its slot booked, where it held the whole worker before. A thread that ends
//! without an answer (it panicked) loses the run it was given: the next removal finds the
//! lease by its sidecar, which is removed last, and takes apart what is left as a leftover.
//!
//! [`Runtime::removing`]: crate::lifecycle::Runtime::removing
//! [`Runtime::remove`]: crate::lifecycle::Runtime::remove

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use js_contain::oci::Oci;
use tokio::runtime::Runtime as TokioRuntime;

use super::fs::Releaser;
use super::ledger::Ledger;
use super::pull::Images;
use super::{CrunRuntime, Run, State, dirs, rootfs};
use crate::lifecycle::{Removal, RunHandle, RuntimeError, Wake};
use crate::pool::{Pending, Poll, Pool, THREADS};

/// What a thread of the removal pool answers: nothing for a lease that is gone, or the
/// failure and the run, for the worker to keep.
type Answer = Result<(), (RuntimeError, Box<Run>)>;

/// The removals under way: by lease name, the answer to come.
pub(super) struct Removals {
    pool: Pool,
    under_way: BTreeMap<String, Pending<Answer>>,
}

impl Removals {
    /// The pool of [`THREADS`] threads, with nothing under way.
    ///
    /// # Errors
    /// A thread could not be made.
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self {
            pool: Pool::named(THREADS, "js-remove")?,
            under_way: BTreeMap::new(),
        })
    }

    /// Rings `wake` whenever a removal begun from now on has answered.
    pub(super) fn set_wake(&mut self, wake: Wake) {
        self.pool.set_wake(wake);
    }

    /// Whether a removal of the lease `name` is under way.
    pub(super) fn holds(&self, name: &str) -> bool {
        self.under_way.contains_key(name)
    }
}

/// What the removal of a lease needs, shared with the threads of the removal pool: shares of
/// what is safe to use from many threads at once and copies of the kill wait and the force
/// wait.
struct Remover {
    oci: Arc<Oci>,
    images: Arc<Images>,
    rt: Arc<TokioRuntime>,
    kill_wait: Duration,
    force_wait: Duration,
    ledger: Ledger,
    /// What unmounts the lease's file system; none for a runtime that gives none.
    mounts: Option<Releaser>,
}

impl CrunRuntime {
    /// What a removal takes a lease apart with.
    fn remover(&self) -> Remover {
        Remover {
            oci: Arc::clone(&self.oci),
            images: Arc::clone(&self.images),
            rt: Arc::clone(&self.rt),
            kill_wait: self.hooks.kill_wait,
            force_wait: self.hooks.force_wait,
            ledger: self.ledger.clone(),
            mounts: self.fs.as_ref().map(super::fs::Fs::releaser),
        }
    }

    /// Removes the container `id` and everything its lease made, on the caller's thread
    /// ([`Runtime::remove`](crate::lifecycle::Runtime::remove)). A handle this process does
    /// not hold is a leftover of an earlier one: it is removed if the ledger holds it as this
    /// agent's, nothing happens if the ledger does not know it, and the call fails (and
    /// changes nothing) if its sidecar is another agent's or is not well formed, or a create
    /// or a removal of it is under way. A removal that fails keeps the run in the books.
    pub(super) fn remove_now(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        let Some(run) = self.runs.remove(&id.0) else {
            // Not held by this process: the leftover of an earlier one, if the ledger has
            // it, and nothing otherwise.
            return self.remove_leftover(&id.0).map(|_| ()).map_err(|why| {
                RuntimeError::new(format!("remove the leftovers of {}: {why}", id.0))
            });
        };
        let removed = self.remover().remove(&id.0, Box::new(run));
        removed.map_err(|(why, run)| {
            self.runs.insert(id.0.clone(), *run);
            why
        })
    }

    /// Begins the removal of the container `id` on a thread of the removal pool, or follows
    /// the one under way ([`Runtime::removing`](crate::lifecycle::Runtime::removing)): once it
    /// answered, the run is out of the books for good, or back in them when the removal
    /// failed, and the removal is forgotten. A handle this process does not hold is removed
    /// on the caller's thread, as [`CrunRuntime::remove_now`] does.
    pub(super) fn remove_off_worker(&mut self, id: &RunHandle) -> Removal {
        if let Some(pending) = self.removals.under_way.remove(&id.0) {
            return match pending.poll(0) {
                Poll::Waiting => {
                    self.removals.under_way.insert(id.0.clone(), pending);
                    Removal::Pending
                }
                Poll::Answered(answer) => Removal::Done(answer.map_err(|(why, run)| {
                    self.runs.insert(id.0.clone(), *run);
                    why
                })),
                // A thread that never answered (it panicked) lost the run: the next removal
                // takes the lease apart by its sidecar.
                Poll::Unanswered(why) => {
                    let why = format!("remove {}: the removal {why}", id.0);
                    Removal::Done(Err(RuntimeError::new(why)))
                }
            };
        }
        let Some(run) = self.runs.remove(&id.0) else {
            return Removal::Done(self.remove_now(id));
        };
        let (remover, name) = (self.remover(), id.0.clone());
        let pending = self
            .removals
            .pool
            .run(u64::MAX, move || remover.remove(&name, Box::new(run)));
        self.removals.under_way.insert(id.0.clone(), pending);
        Removal::Pending
    }
}

impl Remover {
    /// Takes the lease `name` of `run` apart: nothing is answered once it is gone, or the
    /// failure and the run.
    fn remove(&self, name: &str, mut run: Box<Run>) -> Answer {
        self.take_apart(name, &mut run).map_err(|why| (why, run))
    }

    /// The steps of a removal, in order; the first that fails ends it.
    fn take_apart(&self, name: &str, run: &mut Run) -> Result<(), RuntimeError> {
        let (rt, kill_wait, force_wait) = (&self.rt, self.kill_wait, self.force_wait);
        let failed =
            |what: &str, why: &dyn std::fmt::Display| RuntimeError::new(format!("{what}: {why}"));
        if let State::Running { run: running, .. } = &mut run.state
            && running.try_end().is_none()
        {
            // Removed while it runs: ended by force, as a stop that spent its grace.
            let _ = run.cgroup.kill();
            let ended =
                rt.block_on(async { tokio::time::timeout(kill_wait, running.wait()).await });
            if ended.is_err() {
                let forced =
                    rt.block_on(async { tokio::time::timeout(force_wait, running.force()).await });
                forced.map_err(|_| {
                    failed(
                        "end the supervisor",
                        &format!("process {} is still there", running.id().pid),
                    )
                })?;
            }
        }
        run.net.release(name);
        // No progress record is written from here on: a file made in the lease directory while
        // it is removed below would stop the removal.
        run.logs.leave();
        self.oci
            .remove(name)
            .map_err(|why| failed("delete the container", &why))?;
        // After `crun delete` and before the lease directory, which holds the writable layers.
        let released = self.mounts.as_ref().map_or(Ok(()), |fs| fs.release(name));
        released.map_err(|why| failed("unmount the run's file system", &why))?;
        run.cgroup
            .remove()
            .map_err(|why| failed("remove the lease's cgroup", &why))?;
        if let Err(why) = js_contain::remove::remove_tree(&run.lease.dir)
            && why.kind() != io::ErrorKind::NotFound
        {
            return Err(failed("remove the lease directory", &why));
        }
        dirs::remove_tmp_beside(&run.lease.dir, name)
            .map_err(|why| failed("remove the job's temporary directory", &why))?;
        rootfs::unpin_lease(&self.images, &self.oci, name)
            .map_err(|why| failed("remove the image's pin", &why))?;
        self.ledger
            .forget(name)
            .map_err(|why| failed("remove the lease's sidecar", &why))
    }
}
