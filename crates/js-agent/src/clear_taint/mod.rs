//! `js-agent clear-taint`: an operator's clear of the agent's taint, the steps of the manual
//! clear done as one action and recorded.
//!
//! ```text
//! js-agent clear-taint --reason TEXT --by OPERATOR [--state-root DIR] [--scratch-root DIR] [--unit NAME]
//! ```
//!
//! Run as root on a Linux host whose agent runs with the crun runtime (the defaults are the
//! paths and the unit of the packaged `js-agent-crun.service`; give the unit's own when they
//! differ). A taint the agent does not clear itself (see [`taint`](crate::lifecycle::taint))
//! ends only when the host is reimaged or an operator moves the marker aside; this is that move,
//! made the one way:
//!
//! 1. The state root is opened as the agent opens it, and a lock in it
//!    ([`LOCK_FILE`]) is taken, so two clears never run at once.
//! 2. What is to be cleared is read: the marker (`taint.json`), the pin file (`taint.pinned`) and
//!    the records of owed checks (`owed-*`). When there is none of them the host is not tainted:
//!    nothing is done and the unit is not touched. A pin file alone (no marker, no owed record) is
//!    a stale one of a taint already cleared, which the agent ignores and never writes while it
//!    is untainted: it is kept aside and recorded (steps 5 and 6) and the unit is not touched.
//!    A host job's taint (`host-job`) is refused: that machine is reimaged, not cleared.
//! 3. The agent must hold no run that lives: the crun ledger beside the scratch root (one
//!    sidecar for each lease, written before the lease is made and removed after the last of it
//!    is gone) must be empty. The agent makes the ledger when it starts, so a ledger that is not
//!    there (a `--scratch-root` that is not the agent's) fails the clear. A run whose checks found the taint (`Blocked`) has no lease left
//!    and does not count. A run that lives defers the clear (exit 75): a tainted agent starts
//!    nothing, so the run is one that began before the taint, and the clear waits for it to end.
//! 4. The unit is stopped, and waited for (at most [`STOP_WAIT`]). A tainted agent that holds
//!    nothing that lives exits at once on its stop (its drain does not wait on `Blocked` runs).
//!    A unit that is still not down then is started again and the clear fails, with nothing
//!    moved. The ledger is read again once it is down: a lease made in between (an agent that
//!    was not tainted in memory, with only a pin file or an owed record on disk) starts the
//!    unit again and defers the clear, with nothing moved.
//! 5. The files are kept, never removed, under names that end `.cleared-<unix seconds>-by-<operator>`
//!    ([`keep`]): each `owed-<run>` first, then `taint.pinned`, then `taint.json` last, so that a
//!    clear cut short leaves the marker, and with it the taint, until it is run again. A move that
//!    fails leaves the unit stopped: what was kept before it may be the pin file or the owed
//!    records that keep the taint from clearing itself, and the agent is not started on the rest.
//! 6. The clear is recorded in the state root, as `taint.cleared-<unix seconds>-by-<operator>.json`:
//!    when, by whom, why, the taint the marker held, and each file and the name it is kept
//!    under. One line on standard error says the same for the unit's journal. The record's sync of
//!    the directory also makes the moves durable.
//! 7. The unit is started and waited for (at most [`START_WAIT_SECS`]): the agent starts with no
//!    taint, and from a new baseline of the host.
//!
//! The taint the lease server keeps for an agent-made taint ends with the agent's own word: the
//! restarted agent holds no marker, so its first heartbeat says `tainted: false`, and a server
//! that knows the field forgets the taint the agent made (which is why the agent's files go
//! first). A taint the server made itself (`host-job`, `host-unsafe`) and any taint on a server
//! or an agent from before the field end only with the operator's own clear (`ClearHostTaint`).
//! That call is the operator's, with the operator's credential; the verb, which runs with the
//! host's, does not make it.
//!
//! The verb speaks the host updater's hook contract, so hostkeeper can run it as it runs an
//! update step: the exit status (0 success, 1 failure, 75 deferred: nothing was changed, try
//! later) and the last line of standard output, a JSON object ([`Report`]) with a reason token
//! ([`Token`]) and a detail. A failure also writes its detail to standard error.

mod keep;

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use js_update_hook::{
    Clock, Outcome, POLL_SECS, ReasonToken, Report, START_WAIT_SECS, SystemClock, SystemUnits,
    Units,
};

use crate::SystemCommands;
use crate::cli_runtime::CRUN_STORE_DIR;
use crate::lifecycle::StateRoot;
use crate::lifecycle::taint::{MARKER_FILE, PIN_FILE, reason};
use keep::{Operator, Stamp};

/// The verb: `js-agent clear-taint`.
pub(crate) const VERB: &str = "clear-taint";

/// The lock file in the state root that a clear holds while it runs.
pub(crate) const LOCK_FILE: &str = "clear-taint.lock";

/// The directory of the crun ledger's sidecars, in the store beside the scratch root.
const LEDGER_DIR: &str = "oci-labels";

/// How long the stop of the unit is waited for.
pub(crate) const STOP_WAIT: Duration = Duration::from_secs(300);

/// When a deferred clear may be tried again: a run that lives takes minutes to end.
const RETRY_AFTER_SECS: u64 = 300;

/// The most bytes of `--reason`.
const MAX_REASON: usize = 512;

/// The verb's flags.
#[derive(Debug, Parser)]
#[command(
    name = "js-agent clear-taint",
    about = "Clears the agent's taint: keeps the marker, the pin file and the owed records \
             aside, records the clear and restarts the agent",
    disable_version_flag = true
)]
struct Args {
    /// Why the taint is cleared, for the record: what was found and why the host is clean
    /// (at most 512 bytes, no control characters).
    #[arg(long, value_name = "TEXT")]
    reason: String,
    /// Who clears it, for the record and the names the files are kept under: 1 to 64 of
    /// `A-Z a-z 0-9 . _ @ -`, not starting with `.` or `-`.
    #[arg(long, value_name = "OPERATOR")]
    by: String,
    /// The agent's state root (its `--state-root`).
    #[arg(long, value_name = "DIR", default_value = "/var/lib/js-agent/state")]
    state_root: PathBuf,
    /// The agent's scratch root (its `--scratch-root`): the crun ledger is in the store beside
    /// it.
    #[arg(
        long,
        value_name = "DIR",
        default_value = "/var/lib/js-agent/scratch/runs"
    )]
    scratch_root: PathBuf,
    /// The agent's unit, stopped and started again.
    #[arg(long, value_name = "NAME", default_value = "js-agent-crun.service")]
    unit: String,
}

/// The reason a clear did not succeed, as the last line names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Token {
    /// A flag is wrong; nothing was done.
    BadArgs,
    /// The state root cannot be opened or read, or the ledger cannot be listed; nothing was
    /// done.
    StateInvalid,
    /// Another clear holds the lock; nothing was done.
    Locked,
    /// The taint is a host job's: the machine is reimaged, not cleared; nothing was done.
    HostJob,
    /// The agent holds a run that lives; nothing was changed.
    RunHeld,
    /// The unit could not be stopped; nothing was moved and the unit was started again.
    StopFailed,
    /// A file could not be kept aside; the detail says which were, and the unit is left
    /// stopped (the marker is still there).
    ClearFailed,
    /// The files were kept aside but the record could not be written; the unit was started
    /// again.
    RecordFailed,
    /// The taint was cleared and recorded, but the unit did not start.
    StartFailed,
}

impl ReasonToken for Token {
    fn as_str(self) -> &'static str {
        match self {
            Token::BadArgs => "bad_args",
            Token::StateInvalid => "state_invalid",
            Token::Locked => "locked",
            Token::HostJob => "host_job",
            Token::RunHeld => "run_held",
            Token::StopFailed => "stop_failed",
            Token::ClearFailed => "clear_failed",
            Token::RecordFailed => "record_failed",
            Token::StartFailed => "start_failed",
        }
    }
}

/// Every [`Token`], for the test of their spelling.
#[cfg(test)]
const TOKENS: [Token; 9] = [
    Token::BadArgs,
    Token::StateInvalid,
    Token::Locked,
    Token::HostJob,
    Token::RunHeld,
    Token::StopFailed,
    Token::ClearFailed,
    Token::RecordFailed,
    Token::StartFailed,
];

/// How the verb ended: the exit status and the last line.
#[derive(Debug, PartialEq, Eq)]
struct Done {
    outcome: Outcome,
    report: Report<Token>,
}

impl Done {
    fn success(detail: String) -> Self {
        Self {
            outcome: Outcome::Success,
            report: Report {
                detail: Some(detail),
                ..Report::default()
            },
        }
    }

    fn failure(token: Token, detail: String) -> Self {
        Self {
            outcome: Outcome::Failure,
            report: Report::failure(token, detail),
        }
    }

    /// Nothing was changed; try again later.
    fn deferred(token: Token, detail: String) -> Self {
        Self {
            outcome: Outcome::Deferred,
            report: Report {
                retry_after_secs: Some(RETRY_AFTER_SECS),
                ..Report::failure(token, detail)
            },
        }
    }
}

/// What the verb does to the machine besides the state root: the unit manager and time.
pub(crate) struct Host<'a> {
    /// The unit manager.
    pub(crate) units: &'a dyn Units,
    /// Time: the clear's stamp, and the waits for the unit.
    pub(crate) clock: &'a dyn Clock,
}

/// Runs the verb on this machine: `systemctl` and the system's clock.
pub(crate) fn run_system(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let commands = SystemCommands::default();
    let units = SystemUnits::new(&commands);
    let host = Host {
        units: &units,
        clock: &SystemClock,
    };
    run(args, out, err, &host)
}

/// Runs the verb on `args` (the program name and the verb first) over `host`.
pub(crate) fn run(
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
    host: &Host<'_>,
) -> ExitCode {
    let words = args.iter().skip(2).cloned();
    let named = std::iter::once(OsString::from("js-agent clear-taint")).chain(words);
    let args = match Args::try_parse_from(named) {
        Ok(args) => args,
        Err(help) if !help.use_stderr() => {
            let _ = write!(out, "{help}");
            return ExitCode::SUCCESS;
        }
        Err(wrong) => {
            let detail = wrong.to_string().trim_end().to_owned();
            return finish(Done::failure(Token::BadArgs, detail), out, err);
        }
    };
    finish(clear(&args, host, err), out, err)
}

/// Writes how the verb ended: the detail of a failure on `err`, the last line on `out`.
fn finish(done: Done, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    if done.outcome != Outcome::Success {
        let token = done.report.reason.map_or("", Token::as_str);
        let detail = done.report.detail.as_deref().unwrap_or_default();
        let _ = writeln!(err, "js-agent clear-taint: {token}: {detail}");
    }
    let _ = done.report.write_to(out);
    done.outcome.into()
}

/// The clear, steps 1 to 7 of the module documentation.
fn clear(args: &Args, host: &Host<'_>, err: &mut dyn Write) -> Done {
    let who = match (Operator::parse(&args.by), why(&args.reason)) {
        (Ok(who), Ok(())) => who,
        (Err(wrong), _) | (_, Err(wrong)) => return Done::failure(Token::BadArgs, wrong),
    };
    let root = match StateRoot::open(&args.state_root) {
        Ok(root) => root,
        Err(wrong) => return Done::failure(Token::StateInvalid, wrong),
    };
    let _lock = match lock(root.path()) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            let detail = format!("another clear holds {LOCK_FILE}");
            return Done::failure(Token::Locked, detail);
        }
        Err(wrong) => return Done::failure(Token::StateInvalid, format!("{LOCK_FILE}: {wrong}")),
    };
    let found = match Found::read(&root) {
        Ok(found) => found,
        Err(wrong) => return Done::failure(Token::StateInvalid, wrong),
    };
    if found.is_empty() {
        return Done::success(
            "the host is not tainted: there is no marker, pin file or owed record".to_owned(),
        );
    }
    if found
        .taint
        .as_ref()
        .is_some_and(|taint| taint.reason == reason::HOST_JOB)
    {
        let detail = "the taint is a host job's: the machine is reimaged, not cleared \
                      (docs/runbooks/host-jobs.md); nothing was done"
            .to_owned();
        return Done::failure(Token::HostJob, detail);
    }
    if found.names == [PIN_FILE] {
        return pin_alone(&root, args, stamp_of(host), &who, err);
    }
    let ledger = ledger_of(&args.scratch_root);
    if let Some(done) = leases_defer(&ledger) {
        return done;
    }
    if let Err(why) = stop(host, &args.unit) {
        let detail = format!("{why}; nothing was moved and the unit is started again");
        let _ = host.units.start(&args.unit);
        return Done::failure(Token::StopFailed, detail);
    }
    if let Some(done) = leases_defer(&ledger) {
        return started(host, &args.unit, done);
    }
    let stamp = stamp_of(host);
    let kept = match keep::keep_all(&root, &found.names, stamp, &who) {
        Ok(kept) => kept,
        Err((kept, wrong)) => {
            // The marker is still there, but what was kept before it may be the pin file or
            // the owed records that keep the taint from clearing itself: the agent is not
            // started on what is left.
            let detail = format!(
                "{wrong}; {}; {} is left stopped: run the clear again once the cause is fixed",
                keep::said(&kept),
                args.unit
            );
            return Done::failure(Token::ClearFailed, detail);
        }
    };
    let done = recorded(&root, args, stamp, &who, found.taint.as_ref(), &kept, err);
    started(host, &args.unit, done)
}

/// The stamp of a clear made now.
fn stamp_of(host: &Host<'_>) -> Stamp {
    Stamp::at(host.clock.now_ms())
}

/// Records a clear that kept `kept` aside, and says so on `err`: how it ended, unless the unit
/// fails to start after it.
fn recorded(
    root: &StateRoot,
    args: &Args,
    stamp: Stamp,
    who: &Operator,
    taint: Option<&js_agent_wire::Taint>,
    kept: &[keep::Kept],
    err: &mut dyn Write,
) -> Done {
    let record = keep::record(root, stamp, who, &args.reason, taint, kept);
    let told = keep::told(stamp, who, &args.reason, taint, kept);
    let _ = writeln!(err, "js-agent clear-taint: {told}");
    match record {
        Ok(name) => Done::success(format!("{told}; recorded in {name}")),
        Err(wrong) => Done::failure(Token::RecordFailed, format!("{told}; the record: {wrong}")),
    }
}

/// The clear of a pin file found with no marker and no owed record: a stale one, of a taint
/// already cleared, which the agent ignores and never writes while it is not tainted. It is
/// kept aside and recorded, and the agent, which holds no taint, is left running.
fn pin_alone(
    root: &StateRoot,
    args: &Args,
    stamp: Stamp,
    who: &Operator,
    err: &mut dyn Write,
) -> Done {
    match keep::keep_all(root, &[PIN_FILE.to_owned()], stamp, who) {
        Ok(kept) => recorded(root, args, stamp, who, None, &kept, err),
        Err((_, wrong)) => Done::failure(Token::ClearFailed, wrong),
    }
}

/// Whether `reason` may be recorded: 1 to [`MAX_REASON`] bytes, no control character.
fn why(reason: &str) -> Result<(), String> {
    let fits = (1..=MAX_REASON).contains(&reason.len());
    match fits && !reason.chars().any(char::is_control) {
        true => Ok(()),
        false => Err(format!(
            "--reason: 1 to {MAX_REASON} bytes with no control character"
        )),
    }
}

/// Takes the lock in the state root: the file, held locked until it is dropped, or `None`
/// when it cannot be locked (another clear holds it).
///
/// # Errors
/// The lock file cannot be opened.
fn lock(root: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(LOCK_FILE))?;
    Ok(file.try_lock().is_ok().then_some(file))
}

/// What the state root holds that a clear keeps aside.
struct Found {
    /// The taint the marker holds (an unreadable marker reads as `marker-unreadable`).
    taint: Option<js_agent_wire::Taint>,
    /// The files to keep, in the order they are kept: each owed record, the pin file, the
    /// marker.
    names: Vec<String>,
}

impl Found {
    fn read(root: &StateRoot) -> Result<Self, String> {
        let owed = root
            .owed()
            .map_err(|wrong| format!("the owed records: {wrong}"))?;
        let mut names: Vec<String> = owed.iter().map(|run| format!("owed-{run}")).collect();
        // A file that is there, a link included; the state root is the caller's own, so a
        // name it cannot look up is not there.
        let there = |name: &&str| fs::symlink_metadata(root.path().join(name)).is_ok();
        let files = [PIN_FILE, MARKER_FILE].into_iter().filter(there);
        names.extend(files.map(str::to_owned));
        Ok(Self {
            taint: root.read(),
            names,
        })
    }

    fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// The crun ledger of the agent whose scratch root is `scratch_root`: its store is beside the
/// scratch root, as the agent places it.
fn ledger_of(scratch_root: &Path) -> PathBuf {
    let parent = scratch_root.parent().unwrap_or(scratch_root);
    parent.join(CRUN_STORE_DIR).join(LEDGER_DIR)
}

/// The sidecars in `ledger`, sorted: one for each lease the agent made and has not yet taken
/// apart. The crun agent makes the ledger when it starts, so a ledger that is not there is a
/// `--scratch-root` that is not the agent's, and an error: read as empty, it would let the
/// clear stop an agent that runs jobs.
fn leases(ledger: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(ledger)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        names.extend(name.strip_suffix(".json").map(str::to_owned));
    }
    names.sort();
    Ok(names)
}

/// The end of a clear that finds a lease in `ledger` (deferred) or cannot list it (failed);
/// `None` when it is empty.
fn leases_defer(ledger: &Path) -> Option<Done> {
    match leases(ledger) {
        Ok(names) if names.is_empty() => None,
        Ok(names) => Some(Done::deferred(
            Token::RunHeld,
            format!(
                "the agent holds a run that lives (lease {}); nothing was changed",
                names.join(", ")
            ),
        )),
        Err(wrong) => Some(Done::failure(
            Token::StateInvalid,
            format!("the ledger {}: {wrong}", ledger.display()),
        )),
    }
}

/// Stops `unit` and waits until it is down (`inactive` or `failed`), at most [`STOP_WAIT`].
///
/// # Errors
/// The stop was refused, or the unit is not down within the wait: the text says which.
fn stop(host: &Host<'_>, unit: &str) -> Result<(), String> {
    if let Err(wrong) = host.units.stop_no_block(unit) {
        return Err(format!("the stop of {unit} failed: {wrong}"));
    }
    let polls = STOP_WAIT.as_secs().div_ceil(POLL_SECS);
    for _ in 0..=polls {
        let down = host
            .units
            .show(unit)
            .is_ok_and(|state| ["inactive", "failed"].contains(&state.active.as_str()));
        if down {
            return Ok(());
        }
        host.clock.pause(Duration::from_secs(POLL_SECS));
    }
    Err(format!(
        "{unit} did not stop within {}s",
        STOP_WAIT.as_secs()
    ))
}

/// Starts `unit` and waits until it is active, at most [`START_WAIT_SECS`]; `done` is how the
/// clear ended, unless it succeeded and the unit did not start.
fn started(host: &Host<'_>, unit: &str, done: Done) -> Done {
    let asked = host.units.start(unit);
    let polls = START_WAIT_SECS.div_ceil(POLL_SECS);
    let mut active = false;
    for _ in 0..=polls {
        active = asked.is_ok() && host.units.is_active(unit).unwrap_or(false);
        if active || asked.is_err() {
            break;
        }
        host.clock.pause(Duration::from_secs(POLL_SECS));
    }
    match (active, done.outcome) {
        (false, Outcome::Success) => {
            let detail = done.report.detail.unwrap_or_default();
            let why = asked.err().map_or_else(
                || format!("not active within {START_WAIT_SECS}s"),
                |wrong| wrong.to_string(),
            );
            Done::failure(
                Token::StartFailed,
                format!("{detail}; {unit} did not start: {why}"),
            )
        }
        _ => done,
    }
}

#[cfg(test)]
mod tests;
