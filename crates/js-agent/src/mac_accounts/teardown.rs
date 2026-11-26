//! Deleting an account: the steps of [`Pool::retire`], and the record of how they ended.

use super::error::Failure;
use super::host::LiveProcess;
use super::ledger::Record;
use super::pool::Pool;
use super::{DELETE_ROUNDS, KILL_ROUNDS, Proof, Step, folder, ipc, launchd, tree};

/// Whether the kill rounds of a uid may ask launchd about it. Asking is not reading:
/// `launchctl print user/<uid>` makes the user domain of any uid it names, and launchd then
/// tries to start that domain's per-user services every few seconds, for ever when the uid
/// has no account. So launchd is asked only about the uid of a record the pool made and has
/// not deleted yet, and never to print a user domain ([`launchd::printed_domains`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Launchd {
    /// The uid's record is there: its domains are booted out and its login domain printed.
    Ask,
    /// No record has the uid (a free slot, a record already deleted, or one never made):
    /// launchd is not asked, and the rounds judge by the uid's processes alone.
    Never,
}

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
        let ask = self.launchd_for(name, uid)?;
        self.end_processes(name, uid, killed, ask)?;
        ipc::remove(&*self.host, name, uid).map_err(|why| Failure::new(Step::Ipc, why))?;
        self.cleanup(name, uid)
    }

    /// Step 1: kills every process of `uid` and boots out its launchd domains until none is
    /// left, removes its crontab and `at` jobs (so no job of the account starts after
    /// this), and does the rounds again, for a job that fired before they went.
    fn end_processes(
        &self,
        name: &str,
        uid: u32,
        killed: &mut Vec<i32>,
        ask: Launchd,
    ) -> Result<(), Failure> {
        self.kill_rounds(uid, killed, ask)?;
        // Removed only once nothing of the account runs: nothing races the walk.
        let schedules = tree::sweep(&self.settings.schedules, name, uid)
            .map_err(|why| Failure::new(Step::Schedules, why));
        self.kill_rounds(uid, killed, ask)?;
        schedules.map(drop)
    }

    /// Whether launchd may be asked about `uid`, the uid of the account `name`: only while
    /// the directory holds a record named `name` and a record with `uid` ([`Launchd`]). A
    /// record of the name under another uid does not let `uid` be asked about.
    ///
    /// # Errors
    /// The directory could not be asked: the rounds do not start, since neither answer can
    /// be taken for granted.
    pub(super) fn launchd_for(&self, name: &str, uid: u32) -> Result<Launchd, Failure> {
        let held = match self.host.in_directory(name) {
            Ok(true) => self.host.uid_in_directory(uid),
            named => named,
        };
        match held {
            Ok(true) => Ok(Launchd::Ask),
            Ok(false) => Ok(Launchd::Never),
            Err(why) => Err(Failure::new(
                Step::Kill,
                format!("looking up the record {name} before ending uid {uid}'s processes: {why}"),
            )),
        }
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

    /// For at most [`KILL_ROUNDS`] rounds: kills every process of `uid`, and with
    /// [`Launchd::Ask`] boots out its domains, then looks at the outcome, until no process is
    /// left and (with `Ask`) its login domain does not print.
    pub(super) fn kill_rounds(
        &self,
        uid: u32,
        killed: &mut Vec<i32>,
        ask: Launchd,
    ) -> Result<(), Failure> {
        let (domains, printed) = match ask {
            Launchd::Ask => (
                launchd::domains(uid).to_vec(),
                launchd::printed_domains(uid).to_vec(),
            ),
            Launchd::Never => (Vec::new(), Vec::new()),
        };
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
            stayed = launchd::still_printing(&*self.host, &printed, &refused);
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

    /// Step 4: the record ([`Pool::delete_record`]), then the home, then the per-user folders
    /// of the uid, emptied and kept (the system protects them from removal, and the next
    /// account of the slot has the same ones). A record that is not there is no error.
    fn remove_account(&self, name: &str, uid: u32) -> Result<(), Failure> {
        self.delete_record(name)?;
        tree::remove_named(&self.settings.homes.join(name), uid)
            .map_err(|why| Failure::new(Step::Home, why))?;
        folder::empty(&self.settings.user_folders, uid, &*self.os)
            .map_err(|why| Failure::new(Step::Folder, why))
    }

    /// Deletes the record `name`, asking the directory at most [`DELETE_ROUNDS`] times with a
    /// pause between two: OpenDirectory has refused root the deletion of an account's record
    /// once ("the current credentials do not have the appropriate privileges") and taken
    /// the same deletion moments later, and a deletion that fails holds the account and
    /// taints the host for good. A deletion is the same call each time (a record already
    /// gone is no error), so asking again changes nothing else. A refusal that lasts, such as
    /// one for want of Full Disk Access, is refused every round, and the last refusal holds
    /// the account at [`Step::Record`] as one refusal did before. Each refusal followed by
    /// another round is logged.
    fn delete_record(&self, name: &str) -> Result<(), Failure> {
        let mut round = 1;
        loop {
            match self.host.delete_account(name) {
                Ok(_) => return Ok(()),
                Err(why) if round < DELETE_ROUNDS => {
                    tracing::warn!(
                        "deleting the record {name} was refused (round {round} of \
                         {DELETE_ROUNDS}): {why}; asking again"
                    );
                    self.host.pause();
                    round += 1;
                }
                Err(why) => {
                    return Err(Failure::new(
                        Step::Record,
                        format!("deleting the record {name}: {why}"),
                    ));
                }
            }
        }
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
