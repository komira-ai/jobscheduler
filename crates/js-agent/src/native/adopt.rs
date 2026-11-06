//! Adopting the native runs an earlier process of the agent started.
//!
//! The journal keeps a [`RunRecord`] of every run the helper started (`native/journal.rs`).
//! [`Runtime::list_labelled`](crate::lifecycle::Runtime::list_labelled) reads the records of
//! the live leases this process does not hold, and asks the helper's `watch` for each: the
//! helper still has the run's process (it outlives the agent's connection), so the answer
//! is its pid and, when it ends, how it ended, which a thread waits for as it does for a run
//! this process started. The run is then listed with the labels its start had and how long
//! it has run, polled, stopped and removed as any other.
//!
//! A run the helper does not know (its process ended and its user was deleted, the helper
//! restarted, or a helper from before `watch`) is listed too, ended ([`Status::Gone`]): the
//! lifecycle finishes it as a job whose run is gone and removes it, which ends its user and
//! marks its lease done, so nothing of it is leaked. A helper that cannot be reached fails
//! the listing: the agent cannot say what it holds.
//!
//! An adopted run whose start pinned a bundle has its bundle pinned again for it; a pin that
//! fails is only logged (the run goes on, as it did before the restart). One whose scratch
//! directory holds a live-log progress record, on a runtime with live logs, has its upload go
//! on from the record (`native/logs.rs`).

use std::sync::mpsc::channel;
use std::thread;

use js_mac_session::client::ClientError;

use super::journal::RunRecord;
use super::{NativeRuntime, Run, exit_of, fail, logs};
use crate::lifecycle::{Found, RunHandle, RuntimeError, Status, label};

impl NativeRuntime {
    /// Takes over the run of every live lease the journal has a run record of and this
    /// process does not hold.
    pub(super) fn adopt_all(&mut self) -> Result<(), RuntimeError> {
        let records = self
            .journal
            .runs()
            .map_err(|why| fail(&"the lease journal", &why))?;
        for (lease, record) in records {
            if !self.runs.contains_key(&lease) {
                let run = self.adopt(&lease, record)?;
                self.runs.insert(lease, run);
            }
        }
        Ok(())
    }

    /// The run of `lease`, found again through the helper's `watch`.
    fn adopt(&mut self, lease: &str, record: RunRecord) -> Result<Run, RuntimeError> {
        let (sender, waiter) = channel();
        let ended = match self.helper.watch(lease) {
            Ok(running) => {
                tracing::info!("native {lease}: adopted the run of pid {}", running.pid);
                thread::spawn(move || {
                    let _ = sender.send(running.wait().ok().map(exit_of));
                });
                None
            }
            Err(ClientError::Io(why)) => return Err(fail(&"watch", &why)),
            Err(why) => {
                self.learn(&why);
                tracing::warn!("native {lease}: the helper has no run of the lease ({why})");
                Some(Status::Gone)
            }
        };
        let pinned = self.repin(lease, record.bundle.as_deref());
        let run_id = record.labels.get(label::RUN).map(String::as_str);
        let logs = logs::adopted(self.logs.as_ref(), &record.scratch, run_id);
        Ok(Run {
            scratch: record.scratch,
            home: record.home,
            outputs: record.outputs,
            collected: None,
            ended,
            waiter,
            labels: record.labels,
            started_ms: record.started_ms,
            _pinned: pinned,
            logs,
        })
    }

    /// The bundle `pin` pinned again for the adopted run of `lease`, when the run has one
    /// and this runtime holds a store.
    fn repin(&self, lease: &str, pin: Option<&str>) -> Option<crate::bundle::Pinned> {
        let (pin, placer) = (pin?, self.bundles.as_ref()?);
        placer
            .pin(pin, lease)
            .inspect_err(|why| tracing::warn!("native {lease}: the bundle is not pinned: {why}"))
            .ok()
    }

    /// Every run this runtime holds, as the lifecycle lists them.
    pub(super) fn found(&self) -> Vec<Found> {
        let now = (self.now_ms)();
        self.runs
            .iter()
            .map(|(lease, run)| Found {
                id: RunHandle(lease.clone()),
                labels: run.labels.clone(),
                running_ms: now.saturating_sub(run.started_ms),
                never_started: false,
            })
            .collect()
    }
}
