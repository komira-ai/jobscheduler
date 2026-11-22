//! A run after its start: polling it, stopping it, and removing it.
//!
//! - **Poll.** The supervisor's exit record says how the job ended
//!   ([`Supervised::try_exit`](crate::mac_supervisor::Supervised::try_exit), which reads the
//!   supervisor's own status when it wrote none): an exit code is the run's, a signal is
//!   128 and the signal. A supervisor that ended with no word of the job, or that cannot be
//!   asked, leaves the run gone. Once a stop's grace has passed and the job is still
//!   running, the poll sweeps the run's processes ([`end::end_processes`]: by the job's audit
//!   session and the account's uid, the supervisor among them), and so does every later poll
//!   until a sweep leaves none; only then is the end read, so a job the sweep ended reads as
//!   its killed supervisor's status (128 and 9). A job that ended within the grace is not
//!   swept by the poll: what it left is the removal's.
//! - **Stop.** `SIGTERM` to the job's process group, so a job may flush (nothing for a run
//!   that has ended: its job is reaped, and its pid may be another's), and the grace's end
//!   noted; a second stop changes neither. The lifecycle owns the wall limit and calls the
//!   stop.
//! - **Removal**, in this order: the run's processes are swept (whether or not the job has
//!   ended: a process it detached may still write into its folder), its declared outputs copied
//!   out of `exec/` into its scratch directory (once: a removal that is tried again does not
//!   copy them again), its account retired (every process of the uid killed, the record and
//!   the home deleted, the per-user folder emptied, all shown gone) and its lease marked done.
//!   A sweep that is not clean does not stop the removal: the retirement kills the uid again,
//!   and holds the account if a process still stays. An account the pool holds fails the
//!   removal (the run is kept, so a later removal sweeps and tries the retirement again);
//!   outputs that could not be copied fail it only as
//!   [`RuntimeError::outputs`], after everything else is done.

use std::io;
use std::time::Duration;

use rustix::process::Signal;

use super::run::{Job, exit_of};
use super::{NativeRuntime, accounts, end, logs, outputs};
use crate::lifecycle::{RunHandle, RuntimeError, Status};
use crate::mac_supervisor::ExitRecord;
use crate::mac_sweep::ProcessTable;

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
                Some(job) if job.sweeping => swept_then_told(&id.0, job, &*self.table),
                Some(job) => match told(&id.0, job.supervised.try_exit()) {
                    None if job.kill_after.is_some_and(|after| now >= after) => {
                        job.sweeping = true;
                        swept_then_told(&id.0, job, &*self.table)
                    }
                    seen => seen,
                },
            };
            if matches!(run.ended, Some(Status::Exited(_))) {
                logs::end(&mut run.logs, &run.scratch);
            }
        }
        run.ended.unwrap_or(Status::Running)
    }

    /// Stops the run `id`: `SIGTERM` to its job's process group now, and its processes swept
    /// at the first poll after `grace`. Only the first stop does either: a later one moves
    /// nothing.
    pub(super) fn stop_run(&mut self, id: &RunHandle, grace: Duration) {
        let now = (self.now_ms)();
        let run = self.runs.get_mut(&id.0).filter(|run| run.ended.is_none());
        let Some(job) = run.and_then(|run| run.job.as_mut()) else {
            return;
        };
        if job.kill_after.is_some() {
            return;
        }
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
        // First, so nothing the job left writes into its folder while the outputs are copied.
        // A sweep that is not clean goes on to the retirement, which kills the uid again.
        if let Some(job) = &run.job {
            let _ = end::end_processes(&id.0, job, &*self.table);
        }
        run.collect(&id.0, max_file_bytes);
        logs::close(&mut run.logs, &run.scratch);
        if let Some(job) = run.job.take() {
            // A job exists only on a runtime whose pool is open.
            if let Some(pool) = &self.pool {
                match accounts::retire(pool, job.account.clone()) {
                    Ok(retired) => self.last_retired = Some((job.account.name.clone(), retired)),
                    Err(why) => {
                        run.job = Some(job);
                        return Err(why);
                    }
                }
            }
        }
        let run = self.runs.remove(&id.0);
        let _ = self.journal.done(&id.0);
        let failed = run.and_then(|run| run.collected).map(|done| done.failed);
        match failed.unwrap_or_default() {
            failed if failed.is_empty() => Ok(()),
            failed => Err(RuntimeError::outputs(format!(
                "outputs not copied: {}",
                outputs::told(&failed)
            ))),
        }
    }
}

/// Sweeps `job`'s processes, and once none is left reads how the job ended (still `None`
/// when the supervisor's end cannot be read yet: the next poll sweeps and reads again).
fn swept_then_told(lease: &str, job: &mut Job, table: &dyn ProcessTable) -> Option<Status> {
    if !end::end_processes(lease, job, table).clean {
        return None;
    }
    told(lease, job.supervised.try_exit())
}

/// How the run ended, as the supervisor was `asked`: `None` while the job runs; a setup that
/// ended no job, or a supervisor that cannot be asked, leaves the run gone.
fn told(lease: &str, asked: io::Result<Option<ExitRecord>>) -> Option<Status> {
    match asked {
        Ok(None) => None,
        Ok(Some(record)) => Some(exit_of(&record.ended).map_or(Status::Gone, Status::Exited)),
        Err(why) => {
            tracing::warn!("native {lease}: the supervisor cannot be asked: {why}");
            Some(Status::Gone)
        }
    }
}
