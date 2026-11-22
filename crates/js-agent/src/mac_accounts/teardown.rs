//! Deleting an account: the steps of [`Pool::retire`], and the record of how they ended.

use super::error::Failure;
use super::host::LiveProcess;
use super::ledger::Record;
use super::pool::Pool;
use super::{KILL_ROUNDS, Proof, Step, folder, ipc, launchd, tree};

/// How many processes a refusal names; it counts the rest.
pub(super) const NAMED: usize = 8;

/// `<count> processes of uid <uid> remain <when>: <pid> (<command>), ...`, naming at most
/// [`NAMED`].
pub(super) fn remaining(uid: u32, when: &str, left: &[LiveProcess]) -> String {
    let mut named: Vec<String> = left
        .iter()
        .take(NAMED)
        .map(|p| format!("{} ({})", p.pid, p.command))
        .collect();
    if left.len() > NAMED {
        named.push(format!("and {} more", left.len() - NAMED));
    }
    format!(
        "{} processes of uid {uid} remain {when}: {}",
        left.len(),
        named.join(", ")
    )
}

impl Pool {
    /// Steps 1 to 6 of a deletion for the account `name` with `uid`: ends everything of the
    /// uid, removes its IPC objects, then deletes the account and shows it gone. `killed`
    /// gets the pids that were alive at each kill. Nothing is recorded in the ledger: see
    /// [`Pool::settle`].
    pub(super) fn teardown(
        &self,
        name: &str,
        uid: u32,
        killed: &mut Vec<i32>,
    ) -> Result<Proof, Failure> {
        self.end_processes(name, uid, killed)?;
        ipc::remove(&*self.host, name, uid).map_err(|why| Failure::new(Step::Ipc, why))?;
        self.cleanup(name, uid)
    }

    /// Step 1: kills every process of `uid` and boots out its launchd domains until none is
    /// left, removes its crontab and `at` jobs (so no job of the account starts after
    /// this), and does the rounds again, for a job that fired before they went.
    fn end_processes(&self, name: &str, uid: u32, killed: &mut Vec<i32>) -> Result<(), Failure> {
        self.kill_rounds(uid, killed)?;
        // Removed only once nothing of the account runs: nothing races the walk.
        let schedules = tree::sweep(&self.settings.schedules, name, uid)
            .map_err(|why| Failure::new(Step::Schedules, why));
        self.kill_rounds(uid, killed)?;
        schedules.map(drop)
    }

    /// The live processes of `uid`.
    fn processes(&self, uid: u32) -> Result<Vec<LiveProcess>, Failure> {
        self.host.processes(uid).map_err(|why| {
            Failure::new(
                Step::Kill,
                format!("listing the processes of uid {uid}: {why}"),
            )
        })
    }

    /// For at most [`KILL_ROUNDS`] rounds: kills every process of `uid`, boots out its
    /// domains, and looks at the outcome, until no process is left and no domain prints.
    pub(super) fn kill_rounds(&self, uid: u32, killed: &mut Vec<i32>) -> Result<(), Failure> {
        let domains = launchd::domains(uid);
        let mut seen = self.processes(uid)?;
        let mut stayed = Vec::new();
        for _ in 0..KILL_ROUNDS {
            killed.extend(seen.iter().map(|process| process.pid));
            self.host.kill_all(uid).map_err(|why| {
                Failure::new(
                    Step::Kill,
                    format!("killing the processes of uid {uid}: {why}"),
                )
            })?;
            let refused = launchd::boot_out(&*self.host, &domains);
            seen = self.processes(uid)?;
            stayed = launchd::still_printing(&*self.host, &domains, &refused);
            if seen.is_empty() && stayed.is_empty() {
                return Ok(());
            }
            self.host.pause();
        }
        let when = format!("after {KILL_ROUNDS} rounds");
        let domains = format!(
            "launchd domains of uid {uid} remain {when}: {}",
            stayed.join(", ")
        );
        Err(match (seen.is_empty(), stayed.is_empty()) {
            (false, true) => Failure::new(Step::Kill, remaining(uid, &when, &seen)),
            (false, false) => Failure::new(
                Step::Kill,
                format!("{}; {domains}", remaining(uid, &when, &seen)),
            ),
            _ => Failure::new(Step::Domains, domains),
        })
    }

    /// Steps 3 to 6, which a made account that never ran needs too: the shared places, the
    /// record, the home and the per-user folders, then the proof.
    pub(super) fn cleanup(&self, name: &str, uid: u32) -> Result<Proof, Failure> {
        tree::sweep(&self.settings.shared, name, uid)
            .map_err(|why| Failure::new(Step::Sweep, why))?;
        self.remove_account(name, uid)?;
        self.prove_gone(name, uid)
    }

    /// Step 4: the record, then the home, then the per-user folders of the uid, emptied and
    /// kept (the system protects them from removal, and the next account of the slot has
    /// the same ones). A record that is not there is no error.
    fn remove_account(&self, name: &str, uid: u32) -> Result<(), Failure> {
        self.host.delete_account(name).map_err(|why| {
            Failure::new(Step::Record, format!("deleting the record {name}: {why}"))
        })?;
        tree::remove_named(&self.settings.homes.join(name), uid)
            .map_err(|why| Failure::new(Step::Home, why))?;
        folder::empty(&self.settings.user_folders, uid, &*self.os)
            .map_err(|why| Failure::new(Step::Folder, why))
    }

    /// Records how the deletion `done` of account `n` ended: `gone` when it was shown, else
    /// `held` at the step it stopped at. A ledger that cannot take the record is the
    /// failure itself, at [`Step::Ledger`].
    pub(super) fn settle(&self, n: u64, done: Result<Proof, Failure>) -> Result<Proof, Failure> {
        match done {
            Ok(proof) => self
                .record(&Record::Gone(n))
                .map(|()| proof)
                .map_err(|why| Failure::new(Step::Ledger, format!("recording gone: {why}"))),
            Err(failed) => match self.record(&Record::Held(n, failed.step.to_string())) {
                Ok(()) => Err(failed),
                Err(why) => Err(Failure::new(
                    Step::Ledger,
                    format!("recording held at {}: {why}; {}", failed.step, failed.why),
                )),
            },
        }
    }
}

#[cfg(test)]
mod tests;
