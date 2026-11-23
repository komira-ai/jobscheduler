//! Ending every process of a native Mac job: by the job's audit session and by its slot
//! account's uid, in rounds, with a record of which rule found what.
//!
//! A native job runs as its own slot account (`mac_accounts`) and may leave its process
//! group, its session and its process tree: a double fork, `setsid`, a spawn with
//! `POSIX_SPAWN_SETSID`, a `nohup` orphan. On the proof Mac a walk of the parent links or a
//! process group found 10 of a job's 13 processes. The supervisor opens a new audit session
//! before it drops to the slot's uid, every process the job starts inherits it, and a
//! process that is not root cannot open another, so a process is taken as the job's when
//! its audit session is the job's ([`Found::Session`], whatever its uid: a set-uid program
//! the job ran is found too) or when its effective or real uid is the slot's
//! ([`Found::UserOnly`]: the per-user daemons launchd starts for an account, whose session
//! is launchd's). Nothing else is ever a target: not pid 0 or 1, not the sweeping process,
//! and no process of another uid outside the job's session, so the tierstore service and
//! every other user of the Mac are untouchable.
//!
//! [`sweep`] lists every process, ends the targets, and lists again, until a round finds
//! none or [`ROUNDS_MAX`] rounds have run: a fork between a listing and its kill is found in
//! the next round. It reads no clock; the pause between rounds is the table's
//! ([`ProcessTable::pause`]). It refuses ids that could turn it on the agent, tierstore or a
//! system account: a uid below the lowest slot uid (`mac_accounts::Slots::LOWEST`, so 0
//! too) or the agent's own ends nothing and says so in the [`Tally`], and a session that is
//! the agent's own, or is not a session number at all, is dropped so the uid rule runs
//! alone ([`job_ids`], and again in [`sweep`] from the listing).
//!
//! The decisions are here, on every platform, and are tested over a fake table; the macOS
//! table (`libproc` for the listing, the supervisor's `asid_of` for the session, `SIGKILL`)
//! is `macos::MacTable`.

mod decide;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
mod table;
#[cfg(test)]
mod tests;

pub use decide::{Found, JobIds, Proc, ROUNDS_MAX, Swept, Tally, Target, job_ids, sweep, targets};
#[cfg(target_os = "macos")]
pub use macos::MacTable;
pub use table::ProcessTable;
