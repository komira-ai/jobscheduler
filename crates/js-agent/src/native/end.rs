//! Ending a run's processes: the sweep of [`crate::mac_sweep`] by the job's audit session and
//! the account's uid, and the `SIGTERM` of a stop.
//!
//! A job may leave its process group, its session and its process tree (a double fork and
//! `setsid`), so its processes are found by the audit session its supervisor opened, which no
//! process of the job can leave, and by the account's uid (effective or real). The supervisor
//! and the job are targets like any process of the uid. The sweep ends the run's processes at
//! a stop's grace (`native/remove.rs`, the poll) and first in a removal, so that nothing writes
//! into the job's folder while its outputs are copied out; the account's retirement then kills
//! the uid again and shows it gone ([`crate::mac_accounts::Pool::retire`]).

use rustix::process::{Pid, Signal, getpid, kill_process_group};

use super::run::Job;
use crate::mac_supervisor::macos::audit::asid_of;
use crate::mac_sweep::{ProcessTable, Swept, Tally, job_ids, sweep};

/// Ends every process of `job` (the run `lease`) that `table` shows, in rounds, and logs what
/// was found by which rule: at `info` when none is left, at `warn` when the sweep is not clean
/// (and why). The tally says whether every process ended; nothing else does.
pub(super) fn end_processes(lease: &str, job: &Job, table: &dyn ProcessTable) -> Tally {
    let own_pid = getpid().as_raw_nonzero().get();
    let ids = job_ids(job.account.uid, job.ready.asid, own_pid, asid_of(own_pid));
    let tally = sweep(table, &ids);
    let session = tally
        .asid
        .map_or_else(|| "none".to_owned(), |asid| asid.to_string());
    let by_session = named(&tally.by_session);
    let by_user = named(&tally.by_user_only);
    let rounds = tally.rounds;
    match &tally.error {
        None => tracing::info!(
            "native {lease}: processes of uid {} ended in {rounds} rounds; by session \
             {session}: [{by_session}]; by uid only: [{by_user}]",
            ids.uid
        ),
        Some(why) => tracing::warn!(
            "native {lease}: processes of uid {} not all ended after {rounds} rounds: {why}; \
             by session {session}: [{by_session}]; by uid only: [{by_user}]",
            ids.uid
        ),
    }
    tally
}

/// `swept` as a log names it: `pid name`, comma-separated.
fn named(swept: &[Swept]) -> String {
    swept
        .iter()
        .map(|process| format!("{} {}", process.pid, process.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Sends `signal` to the process group of `job`'s program, if its pid can name one.
pub(super) fn signal_group(job: &Job, signal: Signal) {
    let Some(group) = Pid::from_raw(job.ready.job_pid) else {
        return;
    };
    if let Err(why) = kill_process_group(group, signal) {
        tracing::debug!("native: {signal:?} to group {}: {why}", job.ready.job_pid);
    }
}
