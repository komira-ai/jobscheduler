//! The bridge from the lifecycle's blocking worker to a pull that is asynchronous: a
//! [`Prefetcher`] runs the pulls (and unpacks) of images on a runtime of its own, and
//! answers the worker's question "is this image here?" without waiting.
//!
//! Why a runtime of its own: the lifecycle worker is a plain thread that must not wait for
//! a pull (a multi-GiB cold pull would stop every other lease's start, poll and stop, and
//! the heartbeat of a worker that does not step), and the agent's own tokio runtime has
//! only `rt` and `signal` for its signal handling. The prefetcher's runtime has its own
//! worker threads, so a pull is bounded by the pull function's own limits and by nothing
//! the worker's clock counts.
//!
//! Single flight: while an image is being fetched, [`Prefetcher::prepare`] for it starts
//! nothing, so two runs that name one digest pull once. A pull that succeeded is
//! remembered (the pull function is only asked again if the image is gone: a runtime
//! that can lose images asks its own store, not this one). A pull that failed is reported
//! once, to the call that finds it; the next call starts a new pull.
//!
//! A pull that panics is a failed pull. The prefetcher must be dropped from a plain
//! thread, not from inside an async task (tokio refuses to drop a runtime there).

#![allow(dead_code, unused_imports)]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use tokio::runtime::{Builder, Runtime as PullRuntime};

use super::runtime::{Prepared, RuntimeError};

/// What a pull function returns: the image is on the host, or why it is not.
pub type PullFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// Where one image stands.
enum Entry {
    Pulling,
    Done(Result<(), String>),
}

#[derive(Default)]
struct Shared {
    images: Mutex<BTreeMap<String, Entry>>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        self.images.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn finish(&self, image: String, result: Result<(), String>) {
        self.lock().insert(image, Entry::Done(result));
        self.changed.notify_all();
    }
}

/// Runs the pulls of images in the background, one at a time for each image.
pub struct Prefetcher {
    runtime: PullRuntime,
    pull: Arc<dyn Fn(String) -> PullFuture + Send + Sync>,
    shared: Arc<Shared>,
}

impl Prefetcher {
    /// A prefetcher whose pulls are `pull(image)`, run on `threads` worker threads of a
    /// runtime of its own.
    ///
    /// # Errors
    /// The runtime could not be made (threads or descriptors are exhausted).
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
            runtime,
            pull: Arc::new(pull),
            shared: Arc::new(Shared::default()),
        })
    }

    /// Whether `image` is here, starting its pull when nothing has and nothing is under
    /// way. Never waits.
    pub fn prepare(&self, _image: &str) -> Prepared {
        let _ = (
            &self.runtime,
            &self.pull,
            &self.shared.changed,
            self.shared.lock().len(),
        );
        Prepared::Ready
    }

    /// [`Prefetcher::prepare`], then, while the image is being fetched, waits for the
    /// fetch to end and asks again: the answer is `Ready` or `Failed`, never `Pending`. For
    /// a caller with nothing else to do, such as a command that pulls one image.
    pub fn wait(&self, image: &str) -> Prepared {
        let first = self.prepare(image);
        if first != Prepared::Pending {
            return first;
        }
        let mut images = self.shared.lock();
        while matches!(images.get(image), Some(Entry::Pulling)) {
            images = self
                .shared
                .changed
                .wait(images)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(images);
        self.prepare(image)
    }
}
