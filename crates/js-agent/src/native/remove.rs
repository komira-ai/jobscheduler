//! A run after its start: polling it, stopping it, and removing it.
//!
//! - **Poll.** The supervisor's exit record says how the job ended
//!   ([`Supervised::try_exit`](crate::mac_supervisor::Supervised::try_exit), which reads the
//!   supervisor's own status when it wrote none): an exit code is the run's, a signal is
//!   128 and the signal. A supervisor that ended with no word of the job, or that cannot be
//!   asked, leaves the run gone. Once a stop's grace has passed and the run has not ended,
//!   each poll ends its processes ([`end::end_processes`]).
//! - **Stop.** `SIGTERM` to the job's process group, so a job may flush (nothing for a run
//!   that has ended: its job is reaped, and its pid may be another's); the lifecycle owns the
//!   wall limit and calls the stop.
//! - **Removal**, in this order: the run's processes are ended, its declared outputs copied
//!   out of `exec/` into its scratch directory (once: a removal that is tried again does not
//!   copy them again), its account retired (every process of the uid killed, the record and
//!   the home deleted, the per-user folder emptied, all shown gone) and its lease marked done.
//!   Between the copy and the retirement, a run with a put target commits its output to the
//!   put ingest, once (`native/put.rs`).
//!   An account the pool holds fails the removal (the run is kept, so a later removal tries
//!   the retirement again); outputs that could not be copied, or a commit that failed, fail
//!   it only as [`RuntimeError::outputs`], after everything else is done.

use std::time::Duration;

use rustix::process::Signal;

use super::{NativeRuntime, accounts, end, logs, outputs, put, run::exit_of};
use crate::lifecycle::{RunHandle, RuntimeError, Status};

impl NativeRuntime {
    /// How the run `id` is now.
    pub(super) fn poll_run(&mut self, id: &RunHandle) -> Status {
        let now = (self.now_ms)();
        let Some(run) = self.runs.get_mut(&id.0) else {
            return Status::Gone;
        };
        if run.ended.is_none() {
            run.ended = match &mut run.job {
                None => Some(Status::Gone),
                Some(job) => match job.supervised.try_exit() {
                    Ok(None) => {
                        if job.kill_after.is_some_and(|after| now >= after) {
                            end::end_processes(job);
                        }
                        None
                    }
                    Ok(Some(record)) => {
                        Some(exit_of(&record.ended).map_or(Status::Gone, Status::Exited))
                    }
                    Err(why) => {
                        tracing::warn!("native {}: the supervisor cannot be asked: {why}", id.0);
                        Some(Status::Gone)
                    }
                },
            };
            if matches!(run.ended, Some(Status::Exited(_))) {
                logs::end(&mut run.logs, &run.scratch);
            }
        }
        run.ended.unwrap_or(Status::Running)
    }

    /// Stops the run `id`: `SIGTERM` to its job's process group now, and its processes ended
    /// at the first poll after `grace`.
    pub(super) fn stop_run(&mut self, id: &RunHandle, grace: Duration) {
        let now = (self.now_ms)();
        let run = self.runs.get_mut(&id.0).filter(|run| run.ended.is_none());
        let Some(job) = run.and_then(|run| run.job.as_mut()) else {
            return;
        };
        end::signal_group(job, Signal::TERM);
        let grace = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX);
        job.kill_after = Some(now.saturating_add(grace));
    }

    /// Removes the run `id` (see the module documentation).
    pub(super) fn remove_run(&mut self, id: &RunHandle) -> Result<(), RuntimeError> {
        let max_file_bytes = self.config.max_file_bytes;
        let Some(run) = self.runs.get_mut(&id.0) else {
            let _ = self.journal.done(&id.0);
            return Ok(());
        };
        // A job whose end was read is reaped: its pid may name another process group by now.
        // The account's retirement kills every process of the uid either way.
        if let (Some(job), None) = (&run.job, run.ended) {
            end::end_processes(job);
        }
        run.collect(&id.0, max_file_bytes);
        logs::close(&mut run.logs, &run.scratch);
        put::commit_run(&mut self.puts, self.put.as_ref(), &id.0, run);
        if let Some(job) = run.job.take() {
            // A job exists only on a runtime whose pool is open.
            if let Some(pool) = &self.pool
                && let Err(why) = accounts::retire(pool, job.account.clone())
            {
                run.job = Some(job);
                return Err(why);
            }
        }
        let run = self.runs.remove(&id.0);
        let _ = self.journal.done(&id.0);
        let put = put::end_run(&mut self.puts, &mut self.committed, &id.0);
        let failed = run.and_then(|run| run.collected).map(|done| done.failed);
        match failed.unwrap_or_default() {
            failed if failed.is_empty() => put,
            failed => Err(RuntimeError::outputs(format!(
                "outputs not copied: {}",
                outputs::told(&failed)
            ))),
        }
    }
}
