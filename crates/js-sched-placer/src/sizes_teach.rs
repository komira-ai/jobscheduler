//! The history a server teaches from the ended jobs its job store holds, read after it serves
//! (`docs/design/sizing.md`, "Where the history lives").
//!
//! A teaching is begun ([`SchedPlacer::begin_teaching`]) when the server starts: that reads the
//! live allocations, takes them as those of the last pass, and from then on holds every run a
//! pass frees instead of teaching it to the history. It also reads the runs of the newest freed
//! allocations, at most [`BRIDGE_PAGES`] pages of them at a start, and teaches them, in the
//! order the jobs ended, to an interim history that the passes size from until the teaching is
//! taught, and that is taught each run a pass frees as the pass frees it (a run learned before
//! any host is known, once one is). So after a start a job books from its class's recent runs,
//! else its fallback's, else its client's, at the first pass, and the cold size only for a
//! class, a fallback and a client with no run among them. The reads of the ended jobs come
//! later, a page at a time ([`Teaching::read_page`]): the freed allocations newest free first,
//! one scan of their entries and batched reads of their rows a page, outside the engine's lock,
//! while the passes place. [`SchedPlacer::teach`] drops the interim history and teaches the
//! history, under the lock, every ended job that was read, in the order the jobs ended (and of
//! two that ended at once, of their ids), and after them the runs the passes held, in the order
//! the passes freed them: the order a server that read everything before its first pass
//! teaches them in, so the history once taught is the same.
//!
//! A job the passes still follow when the runs are taught (its allocation is live at the last
//! pass, or a pass named it freed since the teaching began, its run held or still being read)
//! is left out of the read, which may have found it already freed: the passes teach it, once. A teaching whose read failed or stopped before its
//! last page teaches none of what it read, as a start whose read failed does, and still teaches
//! the runs the passes held.
//!
//! With the profiles in shadow, the runs the passes held are checked against their classes'
//! profiles only when they are taught, so a teaching logs, after the ended jobs and before the
//! held runs' checks, one line saying the checks start: the classes profiled, the ended jobs
//! taught (none for a teaching not read through) and the seconds the teaching took.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use js_jobs::{Booked, IdSource, JobRecord, JobStore, JobStoreError, MAX_PAGE};
use js_state::ResumeToken;
use js_types::sched::FarmTime;
use tracing::warn;

use super::Held;
use crate::place::SchedPlacer;

/// A teaching of the ended jobs, begun ([`SchedPlacer::begin_teaching`]) and not yet taught
/// ([`SchedPlacer::teach`]): the ended jobs read so far, each with what its freed allocation
/// booked, and where the next page of the read begins. While it is not taught every run a pass
/// frees is held, so a teaching begun is always taught, read through or not.
#[derive(Debug)]
#[must_use = "a teaching begun holds every freed run until it is taught"]
pub struct Teaching {
    /// The ended jobs whose allocations were freed, read so far, newest free first.
    ended: Vec<(JobRecord, Booked)>,
    /// Where the next page begins; `None` before the first page and after the last.
    resume: Option<ResumeToken>,
    /// How many pages were read.
    pages: usize,
    /// Whether the last page was read.
    read: bool,
    /// The placer's farm time when the teaching was begun.
    begun_at: FarmTime,
    /// How many runs of the newest freed allocations the begin read for the interim history.
    bridged: usize,
}

impl Teaching {
    /// Reads the next page of the freed allocations `jobs` holds, newest free first (at most
    /// [`MAX_PAGE`], [`JobStore::freed_page`]), keeping each ended job with what its allocation
    /// booked. Answers whether a page remains; once none does, the teaching is read through and
    /// a further call reads nothing.
    ///
    /// # Errors
    /// The first [`JobStoreError`] of the page's reads; what the page read is not kept.
    pub fn read_page<S: js_state::Store, I: IdSource>(
        &mut self,
        jobs: &JobStore<S, I>,
    ) -> Result<bool, JobStoreError> {
        if self.read {
            return Ok(false);
        }
        let page = jobs.freed_page(self.resume.as_ref(), MAX_PAGE)?;
        self.ended.extend(page.runs);
        self.pages += 1;
        self.resume = page.resume;
        self.read = self.resume.is_none();
        Ok(!self.read)
    }

    /// How many pages of jobs were read.
    #[must_use]
    pub fn pages(&self) -> usize {
        self.pages
    }

    /// How many runs of the newest freed allocations the begin read for the interim history
    /// ([`SchedPlacer::begin_teaching`]), those that teach it nothing (a job cancelled before it
    /// started) included.
    #[must_use]
    pub fn bridged(&self) -> usize {
        self.bridged
    }

    /// The placer's farm time when the teaching was begun.
    #[must_use]
    pub fn begun_at(&self) -> FarmTime {
        self.begun_at
    }
}

/// The runs the store holds a page of before another's would be read, at most: the freed
/// allocations newest free first, each page [`MAX_PAGE`] of them. A begin reads this many
/// pages at most ([`SchedPlacer::begin_teaching`]): 10, so 10,000 runs, in 10 scans of the
/// freed entries and 40 batched reads of rows, whatever the history the store keeps.
pub const BRIDGE_PAGES: usize = 10;

/// The runs of the newest `pages` pages of freed allocations `jobs` holds, newest free first;
/// fewer when the store holds fewer, and those read before a read that fails, which is logged.
fn newest_runs<S: js_state::Store, I: IdSource>(
    jobs: &JobStore<S, I>,
    pages: usize,
) -> Vec<(JobRecord, Booked)> {
    let mut runs = Vec::new();
    let mut resume = None;
    for _ in 0..pages {
        let page = match jobs.freed_page(resume.as_ref(), MAX_PAGE) {
            Ok(page) => page,
            Err(error) => {
                warn!(%error, "the newest runs were not read: the jobs sized before the history is taught are sized from fewer runs");
                break;
            }
        };
        runs.extend(page.runs);
        resume = page.resume;
        if resume.is_none() {
            break;
        }
    }
    runs
}

/// The order the runs are taught in: the order the jobs ended, and of two that ended at once,
/// that of their ids.
fn end_order(a: &(JobRecord, Booked), b: &(JobRecord, Booked)) -> Ordering {
    let (a, b) = (&a.0, &b.0);
    (a.ended_at, &a.id).cmp(&(b.ended_at, &b.id))
}

impl SchedPlacer {
    /// Begins a teaching of the ended jobs `jobs` holds (the module docs): reads the live
    /// allocations, takes them as those of the last pass, and holds from now on every run a pass
    /// frees, until the teaching is taught ([`SchedPlacer::teach`]). It reads the runs of the
    /// newest `bridge` pages of freed allocations at most ([`BRIDGE_PAGES`] at a server's start)
    /// and teaches them the interim history the passes size from, in the order the jobs ended,
    /// so a class with a recent run is sized from it at the first pass; what it reads is set by
    /// the live allocations and `bridge`, not by the history the store keeps. A read of those
    /// runs that fails is logged, and the interim history is taught the runs read before it.
    ///
    /// # Errors
    /// The [`JobStoreError`] of the read of the live allocations: nothing is begun, and the
    /// passes teach what they free.
    pub fn begin_teaching<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
        bridge: usize,
    ) -> Result<Teaching, JobStoreError> {
        // The newest runs before the live allocations: a job freed between the two reads is
        // then not live, so no pass names it freed and teaches the interim history it twice.
        let mut newest = newest_runs(jobs, bridge);
        newest.sort_by(end_order);
        let live = jobs.live_allocations()?;
        let mut engine = self.engine();
        let sizes = engine.sizes();
        let _ = sizes.freed(&live);
        let mut held = Held {
            interim: sizes.summaries.clone(),
            ..Held::default()
        };
        for (job, booked) in &newest {
            held.learn(job, *booked, sizes.bounds);
        }
        sizes.held = Some(held);
        Ok(Teaching {
            ended: Vec::new(),
            resume: None,
            pages: 0,
            read: false,
            begun_at: self.now(),
            bridged: newest.len(),
        })
    }

    /// Teaches the history `teaching` (the module docs): when it was read through, each ended
    /// job it read that the passes do not follow, in the order the jobs ended; then each run the
    /// passes held since it was begun, in the order they freed them. From then on each pass
    /// teaches what it frees. With profiles kept (`Curves::Shadow`) it logs, between the two,
    /// that the shadow checks start: the classes profiled, the ended jobs taught and the
    /// seconds since the teaching was begun, on the placer's clock. Answers how many of the
    /// ended jobs read taught a class a memory sample ([`crate::sizes::JobSizes::observe`]); 0
    /// for a teaching not read through, which teaches the held runs alone.
    pub fn teach(&self, teaching: Teaching) -> usize {
        let mut engine = self.engine();
        let sizes = engine.sizes();
        let held = sizes.held.take().unwrap_or_default();
        let followed: BTreeSet<_> = (sizes.live.keys())
            .chain(held.named.iter())
            .cloned()
            .collect();
        let mut ended = match teaching.read {
            true => teaching.ended,
            false => Vec::new(),
        };
        ended.retain(|(job, _)| !followed.contains(&job.id));
        ended.sort_by(end_order);
        let taught = ended
            .iter()
            .filter(|(job, booked)| sizes.observe(job, *booked))
            .count();
        let took = self.now().saturating_duration_since(teaching.begun_at);
        sizes.log_checks_start(ended.len(), took);
        for (job, booked) in &held.runs {
            let _ = sizes.observe_logged(job, *booked, true);
        }
        taught
    }

    /// Teaches the history every ended job `jobs` holds whose allocation is freed, in the order
    /// the jobs ended, at once: a teaching begun, read through and taught
    /// ([`SchedPlacer::begin_teaching`], [`Teaching::read_page`], [`SchedPlacer::teach`]).
    /// Answers how many runs taught a class a memory sample ([`crate::sizes::JobSizes::observe`]):
    /// a job that ended with no measured usage is read and not counted.
    ///
    /// # Errors
    /// The first [`JobStoreError`] of a read: no ended job is taught.
    pub fn learn_ended<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
    ) -> Result<usize, JobStoreError> {
        let mut teaching = self.begin_teaching(jobs, 0)?;
        let read = loop {
            match teaching.read_page(jobs) {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        let taught = self.teach(teaching);
        read.map(|()| taught)
    }
}

#[cfg(test)]
#[path = "sizes_teach_tests.rs"]
mod tests;
