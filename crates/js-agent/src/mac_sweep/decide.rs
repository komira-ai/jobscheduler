//! Which processes are a job's, and the rounds that end them (see the module documentation).

use serde::Serialize;

use super::ProcessTable;

/// The most rounds a sweep runs: each lists every process again, so a fork between a
/// listing and its kill is found in the next one, and a process that never dies ends the
/// sweep, not clean, instead of holding it for good.
pub const ROUNDS_MAX: usize = 50;

/// The lowest uid a slot account may have (the slot range of `mac_accounts::Slots` starts
/// at it or above). Below it are root and the system's own accounts: a sweep for such a uid
/// ends nothing.
pub const FIRST_SLOT_UID: u32 = 501;

/// A process, as the listing sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    /// Its id.
    pub pid: i32,
    /// Its effective uid.
    pub uid: u32,
    /// Its real uid: the slot's for a set-uid program the job ran, whose effective uid is
    /// the program owner's.
    pub ruid: u32,
    /// Its audit session, when it was asked for and could be read.
    pub asid: Option<i32>,
    /// Its name (the kernel's short command name).
    pub name: String,
}

impl Proc {
    /// Whether it runs as `uid`, by its effective or its real uid.
    fn runs_as(&self, uid: u32) -> bool {
        self.uid == uid || self.ruid == uid
    }
}

/// A process the sweep ends, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Its id.
    pub pid: i32,
    /// Its name.
    pub name: String,
    /// The rule that found it.
    pub why: Found,
}

/// A process the sweep ended (or, in [`Tally::remaining`], could not end), as the tally
/// names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Swept {
    /// Its id.
    pub pid: i32,
    /// Its name.
    pub name: String,
}

/// What a job's processes are known by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobIds {
    /// The slot account's uid the job runs as.
    pub uid: u32,
    /// The job's own audit session, when it has one that is not the sweeper's
    /// ([`job_ids`]).
    pub asid: Option<i32>,
    /// The sweeping process, never a target.
    pub own_pid: i32,
}

/// Which rule found a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Found {
    /// It is in the job's audit session, whatever its uid.
    Session,
    /// It is not, but its effective or real uid is the slot's.
    UserOnly,
}

/// The ids to sweep a job by: the slot's `uid`, the job's audit session `ready_asid` as the
/// supervisor's ready record gave it, and the sweeper's own pid and audit session.
///
/// The job's session is dropped, so that the uid rule runs alone, when it is not a session
/// number (0 or negative), when it is the sweeper's own (a supervisor that opened no
/// session of its own leaves the job in the agent's, and the whole agent session would be
/// taken as the job's), and when the sweeper's own session is not known (it could be the
/// same).
#[must_use]
pub fn job_ids(uid: u32, ready_asid: Option<i32>, own_pid: i32, own_asid: Option<i32>) -> JobIds {
    JobIds {
        uid,
        asid: job_session(ready_asid, own_asid),
        own_pid,
    }
}

/// `job`, when it is a session number known not to be `own`.
fn job_session(job: Option<i32>, own: Option<i32>) -> Option<i32> {
    job.filter(|&asid| asid > 0 && own.is_some_and(|own| own != asid))
}

/// The processes of `procs` to end, and why: those in the job's audit session, and those
/// whose effective or real uid is the job's. Never pid 0 or 1, and never the sweeping
/// process.
#[must_use]
pub fn targets(procs: &[Proc], job: &JobIds) -> Vec<Target> {
    procs
        .iter()
        .filter(|p| p.pid > 1 && p.pid != job.own_pid)
        .filter_map(|p| {
            let why = if job.asid.is_some() && p.asid == job.asid {
                Found::Session
            } else if p.runs_as(job.uid) {
                Found::UserOnly
            } else {
                return None;
            };
            Some(Target {
                pid: p.pid,
                name: p.name.clone(),
                why,
            })
        })
        .collect()
}

/// What a sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    /// The audit session the session rule used, `None` when the uid rule ran alone.
    pub asid: Option<i32>,
    /// Processes found by the audit session.
    pub by_session: Vec<Swept>,
    /// Processes found by the uid alone: on a Mac, the per-user daemons launchd starts for
    /// an account (their audit session is launchd's, not the job's), and any process that
    /// left the job's session.
    pub by_user_only: Vec<Swept>,
    /// The processes the last round still found when [`ROUNDS_MAX`] rounds had run.
    pub remaining: Vec<Swept>,
    /// The rounds whose listing was looked at: those that found something, and the one that
    /// found nothing.
    pub rounds: usize,
    /// Whether the last round found nothing: every process of the job has ended.
    pub clean: bool,
    /// Why the sweep ended without a round that found nothing: the ids were refused, the
    /// process table could not be read, or the rounds ran out.
    pub error: Option<String>,
}

impl Tally {
    /// Records one round's targets. Returns whether it found any.
    pub fn round(&mut self, found: &[Target]) -> bool {
        self.rounds += 1;
        for target in found {
            let list = match target.why {
                Found::Session => &mut self.by_session,
                Found::UserOnly => &mut self.by_user_only,
            };
            if !list.iter().any(|swept| swept.pid == target.pid) {
                list.push(swept(target));
            }
        }
        self.clean = found.is_empty();
        !found.is_empty()
    }

    /// Records why the sweep stopped before a round found nothing: it is not clean.
    pub fn failed(&mut self, why: String) {
        self.clean = false;
        self.error = Some(why);
    }
}

/// `target` as the tally names it.
fn swept(target: &Target) -> Swept {
    Swept {
        pid: target.pid,
        name: target.name.clone(),
    }
}

/// Ends every process of the job `ids` names, in rounds: list, end the targets, pause,
/// list again, until a round finds none (clean) or [`ROUNDS_MAX`] rounds have run (not
/// clean, the processes still found named in [`Tally::remaining`]). Reads no clock.
///
/// Kills nothing, and says why in [`Tally::error`], when the uid is below
/// [`FIRST_SLOT_UID`] or is the sweeper's own (effective or real, as the listing shows the
/// process `ids.own_pid`), or when the listing does not show the sweeper at all (its own
/// uid could not be told). A session that is not a session number, or is the sweeper's own
/// as the listing shows it, or that the sweeper's own cannot be told apart from, is dropped
/// and the uid rule runs alone ([`Tally::asid`] says which ran). A listing that fails ends
/// the sweep, not clean, with the reason.
#[must_use]
pub fn sweep(table: &dyn ProcessTable, ids: &JobIds) -> Tally {
    let mut tally = Tally::default();
    if ids.uid < FIRST_SLOT_UID {
        tally.failed(format!(
            "uid {} is not a slot's (slots are {FIRST_SLOT_UID} and up): nothing was ended",
            ids.uid
        ));
        return tally;
    }
    let mut job = *ids;
    let mut found = Vec::new();
    for round in 0..ROUNDS_MAX {
        if round > 0 {
            table.pause();
        }
        let procs = match table.list(job.asid.is_some()) {
            Ok(procs) => procs,
            Err(why) => {
                tally.failed(format!("listing the processes: {why}"));
                return tally;
            }
        };
        let Some(own) = procs.iter().find(|p| p.pid == job.own_pid) else {
            tally.failed(format!(
                "the sweeping process {} is not in the listing, so its own uid is not known",
                job.own_pid
            ));
            return tally;
        };
        if own.runs_as(job.uid) {
            tally.failed(format!("uid {} is the sweeping process's own", job.uid));
            return tally;
        }
        job.asid = job_session(job.asid, own.asid);
        tally.asid = job.asid;
        found = targets(&procs, &job);
        if !tally.round(&found) {
            return tally;
        }
        for target in &found {
            table.kill(target.pid);
        }
    }
    tally.remaining = found.iter().map(swept).collect();
    let names: Vec<String> = found
        .iter()
        .map(|target| format!("{} ({})", target.pid, target.name))
        .collect();
    tally.failed(format!(
        "after {ROUNDS_MAX} rounds these processes remain: {}",
        names.join(", ")
    ));
    tally
}

#[cfg(test)]
mod tests;
