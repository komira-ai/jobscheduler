//! Ending a lease's account, and the accounts the helper would not delete.
//!
//! A run's account is ended by `kill-uid` and then `user-delete`, which deletes the account
//! and shows it gone. A `kill-uid` the helper refuses is an ordinary removal failure: a
//! process of the user may still be alive, so the run stays in the runtime, the lifecycle
//! keeps the job in removal and tries the step again. A `user-delete` the helper refuses
//! after a `kill-uid` that went through (a step failed, or the proof that the account is gone
//! did not hold) leaves the lease live in its ledger with its uid held, and says which step in
//! the refusal. Only that is a held account: no process of the user is left. The agent keeps
//! such a lease in its journal and in a
//! [`HeldAccounts`](crate::lifecycle::held_accounts::HeldAccounts), takes the run out of the
//! runtime, and fails the removal as a held account ([`RuntimeError::account_held`]), which
//! the lifecycle reports; the lifecycle taints the host from two held accounts wherever the
//! runtime counted them ([`Runtime::held_accounts`](crate::lifecycle::Runtime::held_accounts)).
//! The runtime repeats the end for each held lease after each removal that succeeds, and at
//! its start, and a lease the helper then shows gone is done in the journal and no longer
//! held. A failure that is not a refusal (the socket, a helper that needs its boot reset
//! again) is not a hold: the call fails as it always did and the lease stays as it was. The
//! decisions are in [`crate::lifecycle::held_accounts`], which is tested on every platform.
//!
//! The helper does not give a held uid to another lease, so the range it gives from has one
//! uid fewer for each held account: that is the one native job fewer at a time. A Mac agent
//! has no native slot count of its own (its hello carries no `max_leases`), so nothing here
//! counts it.

use std::fmt;

use js_mac_session::client::{Client, ClientError};

use super::{NativeRuntime, fail, logs, outputs};
use crate::lifecycle::RuntimeError;
use crate::lifecycle::held_accounts::{Ending, ending, gone};

/// Why ending a lease's account stopped: the step, and what the helper answered.
#[derive(Debug)]
pub(super) struct Stop {
    /// The helper verb that did not complete.
    verb: &'static str,
    /// What it answered.
    why: ClientError,
}

impl Stop {
    /// Whether the account is held: the helper refused the `user-delete` after a `kill-uid`
    /// that went through. A refused `kill-uid` is not: processes of the user may be alive.
    fn held(&self) -> bool {
        self.verb == USER_DELETE && ending(&self.why) == Ending::Held
    }
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.verb, self.why)
    }
}

/// The helper verb that deletes the account.
const USER_DELETE: &str = "user-delete";

/// The answer of the helper to `verb`, as a step of ending an account: an answer that says
/// nothing is left to end is the end that was wanted.
fn step(verb: &'static str, result: Result<(), ClientError>) -> Result<(), Stop> {
    match result {
        Err(why) if ending(&why) != Ending::Gone => Err(Stop { verb, why }),
        _ => Ok(()),
    }
}

/// Ends the account of `lease`: `kill-uid`, then `user-delete` once no process of it is left.
fn end_lease(helper: &Client, lease: &str) -> Result<(), Stop> {
    step("kill-uid", helper.kill_uid(lease))?;
    step(USER_DELETE, helper.user_delete(lease).map(drop))
}

impl NativeRuntime {
    /// Ends the run `lease`: `kill-uid`, then the copy of the run's outputs and the close of
    /// its logs (which need its processes gone), then `user-delete`. A `kill-uid` the helper
    /// refused leaves the outputs and the logs alone: a process of the user may still run.
    pub(super) fn end_run(&mut self, lease: &str) -> Result<(), Stop> {
        step("kill-uid", self.helper.kill_uid(lease))?;
        let max_file_bytes = self.config.max_file_bytes;
        if let Some(run) = self.runs.get_mut(lease) {
            run.collect(lease, max_file_bytes);
            logs::close(&mut run.logs, &run.scratch);
        }
        step(USER_DELETE, self.helper.user_delete(lease).map(drop))
    }

    /// The failure of a removal of `lease` that stopped at `stop`. A refused `user-delete`
    /// holds the account: the run is taken out of the runtime, its lease stays live in the
    /// journal, and the failure says what the helper said, how many accounts are held and,
    /// when the removal had copied the run's outputs and some could not be copied, which (the
    /// run is gone, so nothing would say it later). Any other failure, a refused `kill-uid`
    /// among them, leaves the run in the runtime, to be removed again.
    pub(super) fn stopped(&mut self, lease: &str, stop: Stop) -> RuntimeError {
        if !stop.held() {
            self.learn(&stop.why);
            return fail(&stop.verb, &stop.why);
        }
        let run = self.runs.remove(lease);
        self.held.hold(lease);
        tracing::warn!("native {lease}: the account is held: {stop}");
        let failed = run.and_then(|run| run.collected).map(|done| done.failed);
        match failed.unwrap_or_default() {
            failed if failed.is_empty() => self.held.error(stop.to_string()),
            failed => self.held.error(format!(
                "{stop}; outputs not copied: {}",
                outputs::told(&failed)
            )),
        }
    }

    /// Ends the user of `lease` after a start that failed, so none is left behind, and says
    /// what could not be done: the start's own error is the first thing the operator needs,
    /// but a user that is left is a uid held in the helper's range. A refused `user-delete`
    /// after a `kill-uid` that went through holds the account. A refused `kill-uid` does not:
    /// a process of the user may be alive, so the lease stays live in the journal, as it
    /// always did, and the next start ends it.
    pub(super) fn discard(&mut self, lease: &str) -> Vec<String> {
        let killed = self.helper.kill_uid(lease);
        let killed_ok = killed.as_ref().map_or_else(gone, |()| true);
        let deleted = self.helper.user_delete(lease).map(drop);
        let mut problems = Vec::new();
        for (verb, result) in [("kill-uid", killed), (USER_DELETE, deleted)] {
            if let Err(why) = result {
                if verb == USER_DELETE && killed_ok && ending(&why) == Ending::Held {
                    self.held.hold(lease);
                }
                problems.push(format!("{verb}: {why}"));
            }
        }
        problems
    }

    /// Ends the leases an earlier process left that have no run to adopt, and says what was
    /// done and what was not. A lease whose end failed stays in the journal and is held.
    pub(super) fn end_unadopted(&mut self) -> Vec<String> {
        let live = self.journal.live().and_then(|live| {
            let runs = self.journal.runs()?;
            Ok(live
                .into_iter()
                .filter(move |lease| !runs.contains_key(lease)))
        });
        let live = match live {
            Ok(live) => live,
            Err(why) => return vec![format!("native: cannot read the lease journal: {why}")],
        };
        let mut lines = Vec::new();
        let mut retired = Vec::new();
        for lease in live {
            match end_lease(&self.helper, &lease) {
                Ok(()) => {
                    lines.push(format!(
                        "ended the lease user {lease} an earlier process left"
                    ));
                    self.held.release(&lease);
                    retired.push(lease);
                }
                Err(stop) => {
                    self.learn(&stop.why);
                    // A refused `kill-uid` counts as held here too: no run is attached to
                    // the lease (the run of an earlier process is not adopted), so nothing
                    // is forgotten that a process could still use, and the lease stays live
                    // in the journal to be ended again at the next start or after a removal.
                    lines.push(format!("cannot end the lease user {lease}: {stop}"));
                    self.held.hold(&lease);
                }
            }
        }
        if let Err(why) = self.journal.retire(&retired) {
            lines.push(format!("native: cannot rewrite the lease journal: {why}"));
        }
        lines
    }

    /// Tries again to end each held account, after a removal that succeeded: a lease the
    /// helper now shows gone is done in the journal and given up, the others stay held. A
    /// lease whose `kill-uid` is refused stays held as well: it has no run attached, so
    /// keeping it live in the journal to be tried again is all there is to do.
    pub(super) fn retry_held(&mut self) {
        let helper = &self.helper;
        let retried = self
            .held
            .retry(|lease| end_lease(helper, lease).map_err(|stop| stop.to_string()));
        for lease in &retried.ended {
            tracing::info!("native {lease}: the held account is deleted");
            if let Err(why) = self.journal.done(lease) {
                tracing::warn!("native {lease}: the lease journal is not told: {why}");
            }
        }
        for (lease, why) in &retried.failed {
            tracing::warn!("native {lease}: the account is still held: {why}");
        }
    }

    /// Takes the leases the helper's boot reset left held (`held` in its reply): each that the
    /// journal does not know is written to it, so a lease that no process of the agent knew of
    /// is ended by the next start, and held, so it is tried after each removal. A lease the
    /// journal has is ended or adopted as the journal's leases are.
    pub(super) fn take_booted(&mut self, held: Vec<String>) {
        if held.is_empty() {
            return;
        }
        let known = self.journal.live().unwrap_or_default();
        for lease in held.into_iter().filter(|lease| !known.contains(lease)) {
            if let Err(why) = self.journal.add(&lease) {
                tracing::warn!("native {lease}: the held lease is not journaled: {why}");
            }
            tracing::warn!("native {lease}: the boot reset left the account held");
            self.held.hold(&lease);
        }
    }
}
