//! The native runs an earlier process of the agent left: taken back, listed ended, or dropped.
//!
//! A run's supervisor outlives the agent: it is the job's parent, and it writes the exit record
//! with no agent running (`crate::mac_supervisor`). So a restart of the agent need not end a
//! run. In this order, every time the agent starts:
//!
//! 1. **Which runs are taken back** ([`NativeRuntime::adoptable`]), read from the journal before
//!    the pool opens: a run whose record names its supervisor ([`RunRecord::supervisor`]) and
//!    whose exit record is there (the job ended while no agent ran) or whose supervisor is still
//!    that process (`crate::mac_supervisor::adopt::adoptable`).
//! 2. **The reset** (the pool's open, `native/accounts.rs`): the accounts of the jobs taken
//!    back are kept as they are; every other account an earlier process lent is retired. A run
//!    whose account lost its record or home is not kept by the pool either.
//! 3. **The listing** ([`Runtime::list_labelled`](crate::lifecycle::Runtime::list_labelled)):
//!    each run of the journal whose account the pool kept is taken back, with its account, its
//!    supervisor ([`Adopted`], which reads its records), its bundle pinned again, its put target
//!    and its live logs, so it is polled, stopped and removed (its outputs collected and
//!    committed, its account retired) as one this process started. Each other run of the
//!    journal (a record of an earlier release, which names no supervisor; a supervisor gone with
//!    no record; a pool that is not open) is listed ended ([`Status::Gone`], no account, no
//!    outputs), so the lifecycle reports its job failed and removes it, which marks its lease
//!    done.
//! 4. [`Runtime::reclaim`](crate::lifecycle::Runtime::reclaim) drops the leases that have no
//!    run record (a start that died before its job started, whose account the pool's open
//!    retired). Such a start's supervisor starts no job once the agent that started it is gone
//!    (`SuperviseSpec::parent`), so no job runs that the journal does not name.
//!
//! A supervisor taken back that is later found gone with no exit record ends its run gone, as
//! a supervisor this process started ends it when it cannot say how its job ended.
//!
//! A run listed so keeps the live-log upload its scratch directory's progress record names,
//! on a runtime with live logs (`native/logs.rs`), so the output its job wrote is uploaded.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use super::journal::{Journal, PutRecord, RunRecord};
use super::run::{Job, Run};
use super::start::RECORDS_DIR;
use super::{NativeRuntime, accounts, bundle, fail, logs, put};
use crate::lifecycle::{Found, RunHandle, RuntimeError, Status, label};
use crate::mac_accounts::Account;
use crate::mac_put::PutTarget;
use crate::mac_supervisor::adopt::adoptable;
use crate::mac_supervisor::{Adopted, Ready, Supervised as _, SupervisorId};
use crate::mac_usage::RunUsage;

impl NativeRuntime {
    /// The job names (`native/accounts.rs`) of the runs of the journal in `journal` that this
    /// process takes back: those whose record names a supervisor that still runs or that has
    /// left its exit record. A journal that cannot be read takes back none (the listing then
    /// fails, naming it).
    #[must_use]
    pub fn adoptable(journal: &Path) -> BTreeSet<String> {
        let start_of = crate::mac_supervisor::macos::start_of;
        let runs = match Journal::new(journal).runs() {
            Ok(runs) => runs,
            Err(why) => {
                tracing::warn!("native: no run taken back: the lease journal: {why}");
                return BTreeSet::new();
            }
        };
        runs.into_iter()
            .filter(|(_, record)| {
                record
                    .supervisor
                    .is_some_and(|id| adoptable(&record.scratch.join(RECORDS_DIR), id, &start_of))
            })
            .map(|(lease, _)| accounts::job_of(&lease))
            .collect()
    }

    /// Takes back every run of the journal that this process does not hold and whose account
    /// the pool kept, and lists every other one as ended.
    ///
    /// # Errors
    ///
    /// The journal cannot be read: the agent cannot say what it holds.
    pub(super) fn take_back(&mut self) -> Result<(), RuntimeError> {
        let records = self
            .journal
            .runs()
            .map_err(|why| fail(&"the lease journal", &why))?;
        for (lease, record) in records {
            if self.runs.contains_key(&lease) {
                continue;
            }
            let kept = self
                .pool
                .as_ref()
                .and_then(|pool| pool.lent_to(&accounts::job_of(&lease)));
            let run = match (kept, record.supervisor) {
                (Some(account), Some(id)) => {
                    let found = record.put.as_ref().map(put_of);
                    let mut run = self.adopted(&lease, record, account, id);
                    if let Some(found) = found {
                        put::begin(&mut self.puts, &lease, &mut run, found);
                    }
                    run
                }
                (kept, _) => {
                    // An account kept for a run that names no supervisor: nothing can take
                    // the run back, so the account goes now.
                    if let (Some(account), Some(pool)) = (kept, &self.pool)
                        && let Err(why) = accounts::retire(pool, account)
                    {
                        tracing::warn!("native {lease}: {}", why.0);
                    }
                    tracing::warn!(
                        "native {lease}: a run of an earlier process, ended by the restart"
                    );
                    self.ended(record)
                }
            };
            self.runs.insert(lease, run);
        }
        Ok(())
    }

    /// The run `lease` of `record`, taken back with `account`, which the pool kept for it,
    /// and its supervisor `id`.
    fn adopted(&self, lease: &str, record: RunRecord, account: Account, id: SupervisorId) -> Run {
        let kept = Ready {
            job_pid: i32::try_from(record.pid).unwrap_or(0),
            asid: None,
        };
        let dir = record.scratch.join(RECORDS_DIR);
        let mut supervised = Adopted::new(id, dir, kept, Arc::clone(&self.start_of));
        let ready = supervised.wait_ready().unwrap_or(kept);
        tracing::info!(
            "native {lease}: taken back from an earlier process: supervisor {}, job {}, account {}",
            id.pid,
            ready.job_pid,
            account.name
        );
        let pinned = match (&record.bundle, &self.bundles) {
            (Some(pin), Some(placer)) => bundle::pin(placer, pin, lease, None)
                .inspect_err(|why| {
                    tracing::warn!("native {lease}: the bundle is not pinned again: {why}");
                })
                .ok(),
            _ => None,
        };
        let run_id = record.labels.get(label::RUN).map(String::as_str);
        let logs = logs::adopted(self.logs.as_ref(), &record.scratch, run_id);
        Run {
            scratch: record.scratch,
            outputs: record.outputs,
            collected: None,
            ended: None,
            labels: record.labels,
            started_ms: record.started_ms,
            _pinned: pinned,
            logs,
            marks: Vec::new(),
            job: Some(Job {
                account,
                home: record.home,
                supervised: Box::new(supervised),
                ready,
                kill_after: None,
                sweeping: false,
                usage: RunUsage::starting((self.mono_us)(), self.series),
                reaped: None,
            }),
        }
    }

    /// The run of `record` as ended: it has no account and no outputs.
    fn ended(&self, record: RunRecord) -> Run {
        let run_id = record.labels.get(label::RUN).map(String::as_str);
        let logs = logs::adopted(self.logs.as_ref(), &record.scratch, run_id);
        Run {
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
        }
    }

    /// Whether a run of the journal holds an account the pool kept for it at its open: the
    /// self-test then cannot rely on a free uid (`native/start.rs`).
    pub(super) fn holds_adopted(&self) -> bool {
        let Some(pool) = &self.pool else {
            return false;
        };
        self.journal.runs().is_ok_and(|runs| {
            runs.keys()
                .any(|lease| pool.lent_to(&accounts::job_of(lease)).is_some())
        })
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

/// The put target the journal kept of a run, as the runtime commits it.
fn put_of(record: &PutRecord) -> (PutTarget, String) {
    let target = PutTarget {
        dir: record.dir.clone(),
        key_prefix: record.key_prefix.clone(),
    };
    (target, record.output.clone())
}
