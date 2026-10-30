//! The bridge from the lifecycle's blocking worker to a pull that is asynchronous: a
//! [`Prefetcher`] runs the pulls (and unpacks) of images on a runtime of its own, and
//! answers the worker's question "is this image here?" without waiting.
//!
//! Why a runtime of its own: the lifecycle worker is a plain thread that must not wait for
//! a pull (a multi-GiB cold pull would stop every other lease's start, poll and stop, and
//! the heartbeat of a worker that does not step), and the agent's own tokio runtime has
//! only `rt` and `signal` for its signal handling.
//!
//! What bounds the pulls: the number of worker threads bounds the CPU the pulls use, not
//! how many are under way (a pull is a task: one for each distinct image, at most as many
//! as the agent holds runs), and nothing here ends a pull that hangs: **the pull function
//! must have a hard total deadline of its own**, because until its future ends every run
//! that names the image waits on it, and no later call for the image starts another pull.
//!
//! Single flight: while an image is being fetched, [`Prefetcher::prepare`] for it starts
//! nothing, so two runs that name one digest pull once; each is given the ticket of that
//! pull. A pull that succeeded is remembered until [`Prefetcher::forget`] (the owner of an
//! image store that evicts images must call it, or the image is still answered `Ready`).
//! A pull that failed is kept, with its ticket: every call that names that ticket is
//! answered `Failed` (every run that waited on the pull ends), and a call that names no
//! ticket, or another one, starts a new pull.
//!
//! A pull that panics is a failed pull; a pull *function* that panics when it is called
//! leaves no trace of the image, so the next call tries again. Dropping the prefetcher
//! stops its runtime without waiting for the pulls (tokio's `shutdown_background`), so it
//! may be dropped anywhere, an async task included.

#![allow(dead_code, unused_variables, unused_imports)]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use tokio::runtime::{Builder, Handle, Runtime as PullRuntime};

use super::runtime::{Prepared, RuntimeError, Ticket};

/// What a pull function returns: the image is on the host, or why it is not.
pub type PullFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// How one pull stands.
enum Phase {
    Pulling,
    Done(Result<(), String>),
}

/// One image's latest pull.
struct Entry {
    ticket: Ticket,
    phase: Phase,
}

#[derive(Default)]
struct State {
    images: BTreeMap<String, Entry>,
    next: u64,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records how the pull `ticket` of `image` ended, unless the image was forgotten or
    /// pulled again since.
    fn finish(&self, image: &str, ticket: Ticket, result: Result<(), String>) {
        if let Some(entry) = self.lock().images.get_mut(image) {
            entry.phase = Phase::Done(result);
        }
        self.changed.notify_all();
    }
}

/// Runs the pulls of images in the background, one at a time for each image.
#[allow(dead_code)]
pub struct Prefetcher {
    /// Kept to be shut down on drop; pulls are spawned through `handle`.
    runtime: Option<PullRuntime>,
    handle: Handle,
    pull: Arc<dyn Fn(String) -> PullFuture + Send + Sync>,
    shared: Arc<Shared>,
}

impl Prefetcher {
    /// A prefetcher whose pulls are `pull(image)`, run on `threads` worker threads of a
    /// runtime of its own. `pull` must end its future within a limit of its own (see the
    /// [module](self) documentation).
    ///
    /// # Errors
    /// `threads` is 0 ([`std::io::ErrorKind::InvalidInput`]), or the runtime could not be
    /// made (threads or descriptors are exhausted).
    pub fn new(
        threads: usize,
        pull: impl Fn(String) -> PullFuture + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name("js-pull")
            .enable_all()
            .build()?;
        Ok(Self {
            handle: runtime.handle().clone(),
            runtime: Some(runtime),
            pull: Arc::new(pull),
            shared: Arc::new(Shared::default()),
        })
    }

    /// Whether `image` is here, starting its pull when nothing has and nothing is under
    /// way. `waited` is the ticket the caller was given by an earlier `Pending` (none for
    /// a caller that has not asked before): a failure of that pull is reported to it. Never
    /// waits.
    pub fn prepare(&self, image: &str, waited: Option<Ticket>) -> Prepared {
        let mut state = self.shared.lock();
        if let Some(entry) = state.images.get(image) {
            match &entry.phase {
                Phase::Pulling => return Prepared::Pending(entry.ticket),
                Phase::Done(Ok(())) => return Prepared::Ready,
                Phase::Done(Err(why)) => {
                    let failed = Prepared::Failed(RuntimeError::new(why.clone()));
                    state.images.remove(image);
                    return failed;
                }
            }
        }
        // The future is made before the image is marked, so a pull function that panics
        // leaves the image as it was.
        let pulling = (self.pull)(image.to_owned());
        let ticket = Ticket(state.next);
        state.next += 1;
        state.images.insert(
            image.to_owned(),
            Entry {
                ticket,
                phase: Phase::Pulling,
            },
        );
        drop(state);
        let shared = Arc::clone(&self.shared);
        let name = image.to_owned();
        self.handle.spawn(async move {
            // The pull in a task of its own, so a panic in it ends it as a failure here
            // instead of leaving the image `Pulling` for ever.
            let result = tokio::spawn(pulling)
                .await
                .unwrap_or_else(|why| Err(format!("the pull of {name} ended abnormally: {why}")));
            shared.finish(&name, ticket, result);
        });
        Prepared::Pending(ticket)
    }

    /// Forgets what is known of `image`: a success is no longer remembered (the next
    /// call pulls again), a failure is dropped, and a pull under way is let go, its end
    /// ignored. For the owner of an image store that evicts images.
    pub fn forget(&self, image: &str) {
        let _ = image;
    }

    /// [`Prefetcher::prepare`], then, while the image is being fetched, waits for the
    /// fetch to end and asks again naming its ticket: the answer is `Ready` or `Failed`,
    /// never `Pending`. For a caller with nothing else to do, such as a command that pulls
    /// one image.
    pub fn wait(&self, image: &str) -> Prepared {
        let mut waited = None;
        loop {
            match self.prepare(image, waited) {
                Prepared::Pending(ticket) => waited = Some(ticket),
                done => return done,
            }
            let mut state = self.shared.lock();
            while state
                .images
                .get(image)
                .is_some_and(|entry| matches!(entry.phase, Phase::Pulling))
            {
                state = self
                    .shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
    }
}

#[allow(dead_code)]
fn _keep(_: &Prefetcher) {}
