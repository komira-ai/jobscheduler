//! What the server teaches the history the jobs that book nothing are sized from
//! (`js_sched_placer::sizes`, `docs/design/sizing.md` section 7.7) when it starts: every
//! ended job its job store still holds, in the order they ended, so that a restart forgets
//! no size the store's retention keeps. Each pass after it teaches the jobs it sees freed.

use js_jobs::{IdSource, JobStore};
use js_sched_placer::place::SchedPlacer;
use js_state::Store;
use tracing::{info, warn};

/// Teaches `placer` the runs of the ended jobs of `jobs` ([`SchedPlacer::learn_ended`]) and
/// answers how many it taught. A read that fails is logged and the server starts with no
/// history: every class books its cold size until its jobs run again, which is what a server
/// with an empty store does, so the start is not refused for it.
pub(crate) fn learn<S: Store, I: IdSource>(placer: &SchedPlacer, jobs: &JobStore<S, I>) -> usize {
    match placer.learn_ended(jobs) {
        Ok(taught) => {
            info!(
                taught,
                "the job sizes were taught the runs of the ended jobs"
            );
            taught
        }
        Err(error) => {
            warn!(%error, "the ended jobs were not read: every job class starts at its cold size");
            0
        }
    }
}

#[cfg(test)]
#[path = "job_sizes_tests.rs"]
mod tests;
