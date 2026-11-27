//! The history a server teaches from the ended jobs its job store holds, read after it serves
//! (`docs/design/sizing.md`, "Where the history lives").
//!
//! A teaching is begun ([`SchedPlacer::begin_teaching`]) when the server starts: that reads the
//! live allocations alone, takes them as those of the last pass, and from then on holds every
//! run a pass frees instead of teaching it to the history. Its reads of the ended jobs come
//! later, a page at a time ([`Teaching::read_page`]), outside the engine's lock, while the
//! passes place. A job sized meanwhile is sized from an interim history: the history as it stood
//! when the teaching began (nothing, at a start), taught each run a pass frees as the pass frees
//! it (once bounds are known). So after a start a job books the cold size only until a run of its
//! class, its fallback or its client ends and teaches a size, not for as long as the read takes:
//! from then on the runs freed since the start size it, its class's, else its fallback's, else
//! its client's. [`SchedPlacer::teach`] drops the interim history and teaches
//! the history, under the lock, every ended job that was read, in the order the jobs ended, and
//! after them the runs the passes held, in the order the passes freed them: the order a server
//! that read everything before its first pass teaches them in, so the history once taught is the
//! same.
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

use std::collections::BTreeSet;

use js_jobs::{AllocationStep, Booked, IdSource, JobRecord, JobStore, JobStoreError, MAX_PAGE};
use js_state::ResumeToken;
use js_types::sched::FarmTime;

use super::Held;
use crate::place::SchedPlacer;

/// A teaching of the ended jobs, begun ([`SchedPlacer::begin_teaching`]) and not yet taught
/// ([`SchedPlacer::teach`]): the ended jobs read so far, each with what its freed allocation
/// booked, and where the next page of the read begins. While it is not taught every run a pass
/// frees is held, so a teaching begun is always taught, read through or not.
#[derive(Debug)]
#[must_use = "a teaching begun holds every freed run until it is taught"]
pub struct Teaching {
    /// The ended jobs whose allocations were freed, read so far, in key order.
    ended: Vec<(JobRecord, Booked)>,
    /// Where the next page begins; `None` before the first page and after the last.
    resume: Option<ResumeToken>,
    /// How many pages were read.
    pages: usize,
    /// Whether the last page was read.
    read: bool,
    /// The placer's farm time when the teaching was begun.
    begun_at: FarmTime,
}

impl Teaching {
    /// Reads the next page of the jobs `jobs` holds (at most [`MAX_PAGE`]), and the allocation
    /// of each ended one, keeping the jobs whose allocations are freed. Answers whether a page
    /// remains; once none does, the teaching is read through and a further call reads nothing.
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
        let page = jobs.list_jobs(None, self.resume.as_ref(), MAX_PAGE)?;
        let mut freed = Vec::new();
        for job in page.jobs.into_iter().filter(|job| job.result.is_some()) {
            let allocation = jobs.get_allocation(&job.id)?;
            let allocation =
                allocation.filter(|allocation| allocation.step == AllocationStep::Freed);
            freed.extend(allocation.map(|allocation| (job, allocation.booked)));
        }
        self.ended.extend(freed);
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
}

impl SchedPlacer {
    /// Begins a teaching of the ended jobs `jobs` holds (the module docs): reads the live
    /// allocations, takes them as those of the last pass, and holds from now on every run a pass
    /// frees, until the teaching is taught ([`SchedPlacer::teach`]). It reads no ended job: what
    /// it reads is set by the live allocations, not by the history the store keeps.
    ///
    /// # Errors
    /// The [`JobStoreError`] of the read of the live allocations: nothing is begun, and the
    /// passes teach what they free.
    pub fn begin_teaching<S: js_state::Store, I: IdSource>(
        &self,
        jobs: &JobStore<S, I>,
    ) -> Result<Teaching, JobStoreError> {
        let live = jobs.live_allocations()?;
        let mut engine = self.engine();
        let sizes = engine.sizes();
        let _ = sizes.freed(&live);
        sizes.held = Some(Held {
            interim: sizes.summaries.clone(),
            ..Held::default()
        });
        Ok(Teaching {
            ended: Vec::new(),
            resume: None,
            pages: 0,
            read: false,
            begun_at: self.now(),
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
        ended.sort_by_key(|(job, _)| job.ended_at);
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
        let mut teaching = self.begin_teaching(jobs)?;
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
