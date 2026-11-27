//! What the server teaches the history the jobs that book nothing are sized from
//! (`js_sched_placer::sizes`, `docs/design/sizing.md` section 7.7): every ended job its job
//! store still holds, in the order they ended, so that a restart forgets no size the store's
//! retention keeps. Each pass teaches the jobs it sees freed.
//!
//! The start does not wait for it. It begins the teaching ([`begin`]), which reads the live
//! allocations alone, so what it reads is set by the jobs running, not by the history the store
//! keeps; the server then listens and answers ready. The ended jobs are read after that, a page
//! at a time, off the runtime's threads ([`Sizes::teach`]): meanwhile a job that books nothing
//! books what the history taught so far (its fallback's or the cold size, at a start), and the
//! runs the passes free are held, so that once taught the history is the one a server that read
//! everything before its first pass would hold. A stop of the server ([`Sizes::stop`]) ends the
//! read at the next page.
//!
//! How the teaching ended ([`Taught`]) is told to the readiness gate ([`spawn`]), whose route
//! `GET /readyz/sizes` answers 503 `teaching` until then (`crate::ready`): what a deploy reads
//! to know whether replacing the server would throw away a history it is still reading.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use js_sched_placer::place::SchedPlacer;
use js_sched_placer::sizes::Teaching;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::api::Shared;
use crate::job_store::ServerJobs;
use crate::ready::Gate;
use crate::util::blocking;

/// How a teaching of the ended jobs ended ([`Sizes::teach`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Taught {
    /// Every ended job was read and taught; `classes` of them taught a class a memory sample.
    Read {
        /// The ended jobs that taught a class a memory sample.
        classes: usize,
    },
    /// No teaching was begun and none failed: the server keeps no job tables, or the teaching
    /// was taught already.
    Nothing,
    /// The start's read of the live allocations failed, so no teaching was begun.
    AllocationsUnread,
    /// A read of the ended jobs failed: the sizes learn from the jobs that end from now on.
    EndedUnread,
    /// The server stopped before the ended jobs were read.
    Stopped,
}

impl Taught {
    /// The ended jobs that taught a class a memory sample: 0 unless they were all read.
    pub(crate) fn classes(self) -> usize {
        match self {
            Self::Read { classes } => classes,
            Self::Nothing | Self::AllocationsUnread | Self::EndedUnread | Self::Stopped => 0,
        }
    }
}

/// The teaching of the ended jobs a start began and the server has not yet taught, and whether
/// the server is stopping.
#[derive(Default)]
pub(crate) struct Sizes {
    /// The teaching begun, with the placer it teaches and the tables it reads; `None` before a
    /// start began one, when its start could not, and once it is taught.
    begun: Mutex<Option<Begun>>,
    /// Whether the server is stopping, which ends the read at its next page.
    stopping: AtomicBool,
    /// Whether the start's read of the live allocations failed, so that none was begun.
    unread: bool,
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
            unread: false,
        },
        Err(error) => {
            warn!(%error, "the live allocations were not read: every job class starts at its cold size");
            Sizes {
                unread: true,
                ..Sizes::default()
            }
        }
    }
}

impl Sizes {
    /// Reads every ended job of the teaching begun, a page at a time, and teaches the placer
    /// them and the runs the passes held meanwhile ([`SchedPlacer::teach`]); answers how it
    /// ended, with how many taught a class a memory sample when every page was read (a job that
    /// ended with no usage is not counted). It
    /// blocks on the store's reads: the server runs it off the runtime's threads. A read that
    /// fails, or a stop, is logged and teaches the held runs alone, as a start whose read failed
    /// did; the passes teach what they free from then on. Under `--placement-curves shadow` the
    /// placer logs here, read through or not, that the shadow checks start (no run is checked
    /// before). With no teaching begun, or once it is taught, it does nothing and answers
    /// [`Taught::Nothing`], or [`Taught::AllocationsUnread`] when the start's read failed.
    pub(crate) fn teach(&self) -> Taught {
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
            return match self.unread {
                true => Taught::AllocationsUnread,
                false => Taught::Nothing,
            };
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
                Taught::Read { classes: taught }
            }
            Err(Some(error)) => {
                warn!(%error, pages, "the ended jobs were not read: the job sizes learn from the jobs that end from now on");
                Taught::EndedUnread
            }
            Err(None) => {
                info!(
                    pages,
                    "the server stopped before the ended jobs were read: their runs were not taught"
                );
                Taught::Stopped
            }
        }
    }

    /// Ends the read of [`Sizes::teach`] at its next page: what a stop of the server does.
    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }
}

/// Teaches the job sizes the ended jobs the start began to teach ([`Sizes::teach`]), off the
/// runtime's threads, once `hold` (a test's) is sent or dropped (`None`: at once), then tells
/// `gate` how the teaching ended, so that its route `/readyz/sizes` stops answering
/// `teaching`; the task answers how many ended jobs taught a class a memory sample.
pub(crate) fn spawn(
    shared: &Arc<Shared>,
    gate: &Gate,
    hold: Option<oneshot::Receiver<()>>,
) -> JoinHandle<usize> {
    let (shared, gate) = (shared.clone(), gate.clone());
    tokio::spawn(async move {
        if let Some(hold) = hold {
            let _ = hold.await;
        }
        let taught = blocking(move || shared.agents().sizes.teach()).await;
        gate.set_sizes(taught);
        taught.classes()
    })
}

#[cfg(test)]
#[path = "job_sizes_tests.rs"]
mod tests;
