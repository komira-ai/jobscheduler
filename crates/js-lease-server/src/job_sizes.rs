//! What the server teaches the history the jobs that book nothing are sized from
//! (`js_sched_placer::sizes`, `docs/design/sizing.md` section 7.7): every ended job its job
//! store still holds, in the order they ended, so that a restart forgets no size the store's
//! retention keeps. Each pass teaches the jobs it sees freed.
//!
//! The start does not wait for it. It begins the teaching ([`begin`]), which reads the live
//! allocations alone, so what it reads is set by the jobs running, not by the history the store
//! keeps; the server then listens and answers ready. The ended jobs are read after that, a page
//! at a time, off the runtime's threads ([`Sizes::teach`]): meanwhile a job that books nothing
//! is sized from the runs the passes freed since the start (the cold size only until a run of
//! its class, its fallback or its client ends), and those runs are held, so that once taught
//! the history is the one a server that read everything before its first pass would hold. A
//! stop of the server ([`Sizes::stop`]) ends the read at the next page.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use js_sched_placer::place::SchedPlacer;
use js_sched_placer::sizes::Teaching;
use tracing::{info, warn};

use crate::job_store::ServerJobs;

/// The teaching of the ended jobs a start began and the server has not yet taught, and whether
/// the server is stopping.
#[derive(Default)]
pub(crate) struct Sizes {
    /// The teaching begun, with the placer it teaches and the tables it reads; `None` before a
    /// start began one, when its start could not, and once it is taught.
    begun: Mutex<Option<Begun>>,
    /// Whether the server is stopping, which ends the read at its next page.
    stopping: AtomicBool,
}

/// A teaching begun, with what it is read from and taught to.
struct Begun {
    teaching: Teaching,
    placer: Arc<SchedPlacer>,
    jobs: Arc<ServerJobs>,
}

/// Begins the teaching of the ended jobs of `jobs` to `placer`
/// ([`SchedPlacer::begin_teaching`]): reads the live allocations alone. A read that fails is
/// logged, and the server starts with no history: every class books its cold size until its
/// jobs run again, which is what a server with an empty store does, so the start is not
/// refused for it.
pub(crate) fn begin(placer: &Arc<SchedPlacer>, jobs: &Arc<ServerJobs>) -> Sizes {
    match placer.begin_teaching(jobs.as_ref()) {
        Ok(teaching) => Sizes {
            begun: Mutex::new(Some(Begun {
                teaching,
                placer: placer.clone(),
                jobs: jobs.clone(),
            })),
            stopping: AtomicBool::new(false),
        },
        Err(error) => {
            warn!(%error, "the live allocations were not read: every job class starts at its cold size");
            Sizes::default()
        }
    }
}

impl Sizes {
    /// Reads every ended job of the teaching begun, a page at a time, and teaches the placer
    /// them and the runs the passes held meanwhile ([`SchedPlacer::teach`]); answers how many
    /// taught a class a memory sample (a job that ended with no usage is not counted). It
    /// blocks on the store's reads: the server runs it off the runtime's threads. A read that
    /// fails, or a stop, is logged and teaches the held runs alone, as a start whose read failed
    /// did; the passes teach what they free from then on. Under `--placement-curves shadow` the
    /// placer logs here, read through or not, that the shadow checks start (no run is checked
    /// before). With no teaching begun, or once it is taught, it does nothing and answers 0.
    pub(crate) fn teach(&self) -> usize {
        let begun = self
            .begun
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(Begun {
            mut teaching,
            placer,
            jobs,
        }) = begun
        else {
            return 0;
        };
        let read = loop {
            if self.stopping.load(Ordering::SeqCst) {
                break Err(None);
            }
            match teaching.read_page(jobs.as_ref()) {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(error) => break Err(Some(error)),
            }
        };
        let pages = teaching.pages();
        let taught = placer.teach(teaching);
        match read {
            Ok(()) => {
                info!(
                    taught,
                    pages, "the job sizes were taught the runs of the ended jobs"
                );
                taught
            }
            Err(Some(error)) => {
                warn!(%error, pages, "the ended jobs were not read: the job sizes learn from the jobs that end from now on");
                0
            }
            Err(None) => {
                info!(
                    pages,
                    "the server stopped before the ended jobs were read: their runs were not taught"
                );
                0
            }
        }
    }

    /// Ends the read of [`Sizes::teach`] at its next page: what a stop of the server does.
    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
#[path = "job_sizes_tests.rs"]
mod tests;
