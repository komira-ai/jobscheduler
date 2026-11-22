//! The native runs an earlier process of the agent left.
//!
//! A restart of the agent ends every native run: the pool's open retires every account a run
//! of an earlier process held (`native/accounts.rs`), so nothing of those runs is left on the
//! Mac, and nothing watches a supervisor this process did not start. The journal still
//! names them: [`Runtime::list_labelled`](crate::lifecycle::Runtime::list_labelled) lists each
//! lease the journal has a run record of as ended ([`Status::Gone`], no account, no outputs),
//! so the lifecycle reports its job failed and removes it, which marks its lease done.
//! [`Runtime::reclaim`](crate::lifecycle::Runtime::reclaim) drops the leases that have no run
//! record (a start that died before its job started).
//!
//! A run listed so keeps the live-log upload its scratch directory's progress record names,
//! on a runtime with live logs (`native/logs.rs`), so the output its job wrote is uploaded.

use super::run::Run;
use super::{NativeRuntime, fail, logs};
use crate::lifecycle::{Found, RunHandle, RuntimeError, Status, label};

impl NativeRuntime {
    /// Lists every lease of the journal that has a run record and that this process does not
    /// hold as an ended run.
    ///
    /// # Errors
    ///
    /// The journal cannot be read: the agent cannot say what it holds.
    pub(super) fn list_ended(&mut self) -> Result<(), RuntimeError> {
        let records = self
            .journal
            .runs()
            .map_err(|why| fail(&"the lease journal", &why))?;
        for (lease, record) in records {
            if self.runs.contains_key(&lease) {
                continue;
            }
            tracing::warn!("native {lease}: a run of an earlier process, ended by the restart");
            let run_id = record.labels.get(label::RUN).map(String::as_str);
            let logs = logs::adopted(self.logs.as_ref(), &record.scratch, run_id);
            let run = Run {
                scratch: record.scratch,
                outputs: record.outputs,
                collected: None,
                ended: Some(Status::Gone),
                labels: record.labels,
                started_ms: record.started_ms,
                _pinned: None,
                logs,
                marks: Vec::new(),
                job: None,
            };
            self.runs.insert(lease, run);
        }
        Ok(())
    }

    /// Drops from the journal the leases an earlier process left that have no run record:
    /// their starts died before a job started, and the pool's open retired their accounts.
    pub(super) fn drop_unstarted(&mut self) -> Vec<String> {
        let live = self.journal.live().and_then(|live| {
            let runs = self.journal.runs()?;
            Ok(live
                .into_iter()
                .filter(|lease| !runs.contains_key(lease) && !self.runs.contains_key(lease))
                .collect::<Vec<_>>())
        });
        let live = match live {
            Ok(live) => live,
            Err(why) => return vec![format!("native: cannot read the lease journal: {why}")],
        };
        let mut lines: Vec<String> = live
            .iter()
            .map(|lease| {
                format!("dropped the lease {lease} an earlier process left before its job started")
            })
            .collect();
        if let Err(why) = self.journal.retire(&live) {
            lines.push(format!("native: cannot rewrite the lease journal: {why}"));
        }
        lines
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
