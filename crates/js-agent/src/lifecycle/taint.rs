//! The taint marker: the agent's durable record that this host can no longer be trusted
//! clean (design 5.12), and what the lifecycle keeps of it in memory.
//!
//! # The marker
//!
//! One file, [`MARKER_FILE`], in the state root (`--state-root`): a directory the agent's
//! user owns that group and others cannot enter, checked when the agent starts. The file
//! holds the [`Taint`] the heartbeat carries, as JSON.
//!
//! - **It is written atomically and durably.** The text goes to a temporary file in the
//!   same directory, the file is synced, renamed over the marker, and the directory is
//!   synced. A crash leaves the old state or the new one, never half a marker, and a
//!   marker the agent reports is a marker that survives a power cut.
//! - **No code deletes it.** The agent has no path that removes or empties the marker. Only
//!   an operator who reimages the host, or removes the file by hand, ends a taint.
//! - **It is read when the agent starts**, so a tainted agent that restarts is still
//!   tainted. A marker that exists but cannot be read or understood taints the host with the
//!   reason [`reason::MARKER_UNREADABLE`]: a file that is there is never taken for an absent
//!   one.
//! - **The first taint stays.** A host that is tainted already records nothing more; the
//!   report says why it was first tainted.
//! - **It belongs to the state root, not to the machine.** A second agent on the same
//!   machine, an agent started with another `--state-root`, and a state root on a file
//!   system that is lost at a reboot (a `tmpfs`) do not know of it, and a taint ends
//!   silently for them. Run one agent, with one state root on a disk of its own, for each
//!   machine.
//! - **The state root shares no file system with anything a job can fill.** A job that
//!   fills the disk makes the next write of the marker fail with `ENOSPC`; the agent is
//!   then tainted in memory only, and a crash or reboot would lose the taint. The agent
//!   refuses to start when the state root is on the same file system (`st_dev`) as a mount
//!   root or the scratch root ([`Local::check_filesystems`](super::Local::check_filesystems)).
//!
//! The state root holds one more kind of file: `owed-<run id>`, the record that a job's
//! post-job checks are owed (see [`StateRoot::owe`]). It is written, synced, before the
//! first step of the job's cleanup and removed once the checks have run; one found when the
//! agent starts taints the host with the reason [`reason::CHECK_OWED`].
//!
//! # What the lifecycle does with it
//!
//! A tainted agent starts no job (every `Start` is answered `failed_to_start` with the code
//! `host-tainted`, final for that `Start`) and lets the jobs it runs finish.
//! [`Lifecycle::pending_taint`] is what the next heartbeat's `taint` field carries, once the
//! marker is on disk and until the server acknowledges it (`taint_ack`); the
//! acknowledgement stops the report and keeps the marker. A marker that cannot be written
//! does not clear the taint: the agent is tainted in memory, refuses new starts, frees no
//! slot, reports nothing (so a taint the server was told of is one a restart finds) and
//! writes the marker again at every step.
//!
//! [`Lifecycle::pending_taint`]: super::Lifecycle::pending_taint

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use js_agent_wire::Taint;

use super::Event;
use super::scratch::private_dir;

/// The name of the marker file in the state root.
pub const MARKER_FILE: &str = "taint.json";

/// The temporary file the marker is written through, renamed over [`MARKER_FILE`].
pub const MARKER_TEMP_FILE: &str = "taint.json.tmp";

/// The most bytes of a marker that are read. A real marker is a few hundred bytes.
const MARKER_MAX: u64 = 4096;

/// The start of the name of the file that records the checks a run owes: `owed-<run id>`.
const OWED_PREFIX: &str = "owed-";

/// The most records of owed checks that are read when the agent starts. The agent holds at
/// most as many runs as a heartbeat can list, so more records than this is not its own doing.
pub const OWED_MAX: usize = 1024;

/// The `reason` of a [`Taint`] the agent makes.
pub mod reason {
    /// A job that was allowed to change the host (`isolation = host`) was started.
    pub const HOST_JOB: &str = "host-job";
    /// A post-job check found something the job left on the host.
    pub const RESIDUE: &str = "residue";
    /// A post-job check could not run, so the host is not known to be clean.
    pub const UNCHECKED: &str = "unchecked";
    /// A container or a scratch directory could not be removed.
    pub const CLEANUP_FAILED: &str = "cleanup-failed";
    /// A marker was there and could not be read or understood.
    pub const MARKER_UNREADABLE: &str = "marker-unreadable";
    /// The agent started and found the record of a job whose post-job checks were owed
    /// and never ran (the agent stopped between the job's cleanup and its checks), or
    /// could not tell whether there is one.
    pub const CHECK_OWED: &str = "check-owed";
}

/// The state root: the directory the agent keeps what must outlive a restart in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRoot {
    path: PathBuf,
}

impl StateRoot {
    /// Opens the state root at `path`.
    ///
    /// # Errors
    ///
    /// The path is not an existing directory, is a link, is owned by another user, or
    /// grants group or others any permission. The text names the flag.
    pub fn open(path: &Path) -> Result<Self, String> {
        private_dir(path, "--state-root").map(|path| Self { path })
    }

    /// The canonical path of the directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The path of the marker file, which may not exist.
    pub fn marker_path(&self) -> PathBuf {
        self.path.join(MARKER_FILE)
    }

    /// The taint on record: `None` when there is no marker, and the taint of
    /// [`reason::MARKER_UNREADABLE`] when a marker is there that cannot be read, is not
    /// text, is longer than [`MARKER_MAX`] bytes or is not a valid [`Taint`].
    pub fn read(&self) -> Option<Taint> {
        let text = File::open(self.marker_path()).and_then(|file| {
            let mut text = String::new();
            file.take(MARKER_MAX)
                .read_to_string(&mut text)
                .map(|_| text)
        });
        match text {
            Err(why) if why.kind() == io::ErrorKind::NotFound => None,
            Err(_) => Some(unreadable()),
            Ok(text) => Some(
                serde_json::from_str::<Taint>(&text)
                    .ok()
                    .filter(|taint| taint.validate().is_ok())
                    .unwrap_or_else(unreadable),
            ),
        }
    }

    /// Writes `taint` as the marker: to a temporary file, synced, renamed over the marker,
    /// and the directory synced.
    ///
    /// # Errors
    ///
    /// Any step failed. The marker is then what it was before, or the new one: never a
    /// part of it.
    pub fn write(&self, taint: &Taint) -> io::Result<()> {
        let temp = self.path.join(MARKER_TEMP_FILE);
        serde_json::to_vec(taint)
            .map_err(io::Error::other)
            .and_then(|body| {
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&temp)
                    .and_then(|mut file| file.write_all(&body).and_then(|()| file.sync_all()))
            })
            .and_then(|()| fs::rename(&temp, self.marker_path()))
            .and_then(|()| self.sync())
    }
}

impl StateRoot {
    /// The record of the checks owed by `run_id`.
    fn owed_path(&self, run_id: &str) -> PathBuf {
        self.path.join(format!("{OWED_PREFIX}{run_id}"))
    }

    /// Makes what was created or removed in the directory durable.
    fn sync(&self) -> io::Result<()> {
        File::open(&self.path).and_then(|dir| dir.sync_all())
    }

    /// Records that the post-job checks of `run_id` are owed: a file in the state root,
    /// synced with the directory before this returns. `run_id` is a run id the wire accepts,
    /// which holds no path separator.
    ///
    /// # Errors
    ///
    /// The record could not be made durable.
    pub fn owe(&self, run_id: &str) -> io::Result<()> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.owed_path(run_id))
            .and_then(|mut file| {
                file.write_all(run_id.as_bytes())
                    .and_then(|()| file.sync_all())
            })
            .and_then(|()| self.sync())
    }

    /// Records that the checks of `run_id` have run: removes the record. A record that is
    /// not there is already settled.
    ///
    /// # Errors
    ///
    /// The record could not be removed.
    pub fn settle(&self, run_id: &str) -> io::Result<()> {
        match fs::remove_file(self.owed_path(run_id)) {
            Ok(()) => self.sync(),
            Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(why) => Err(why),
        }
    }

    /// The runs whose checks are owed, sorted.
    ///
    /// # Errors
    ///
    /// The state root could not be listed, or holds more than [`OWED_MAX`] records.
    pub fn owed(&self) -> io::Result<Vec<String>> {
        let mut runs = Vec::new();
        for entry in fs::read_dir(&self.path)? {
            let name = entry?.file_name();
            let owed = name.to_string_lossy();
            runs.extend(owed.strip_prefix(OWED_PREFIX).map(str::to_owned));
        }
        runs.sort();
        match runs.len() > OWED_MAX {
            true => Err(io::Error::other(format!(
                "more than {OWED_MAX} records of owed checks"
            ))),
            false => Ok(runs),
        }
    }
}

/// The taint of a marker that is there and cannot be used.
fn unreadable() -> Taint {
    Taint {
        reason: reason::MARKER_UNREADABLE.to_owned(),
        run_id: None,
        check: None,
    }
}

/// What the lifecycle keeps of the taint: the taint, whether the marker holds it, and
/// whether the server has acknowledged the report.
pub(super) struct TaintState {
    root: StateRoot,
    taint: Option<Taint>,
    persisted: bool,
    acked: bool,
}

impl TaintState {
    /// The state on record in `root`: tainted when a marker is there.
    pub(super) fn load(root: StateRoot) -> Self {
        let taint = root.read();
        Self {
            persisted: taint.is_some(),
            taint,
            root,
            acked: false,
        }
    }

    pub(super) fn is_tainted(&self) -> bool {
        self.taint.is_some()
    }

    /// What the next heartbeat reports: the taint, once the marker holds it and until it is
    /// acknowledged. A taint that is not on disk is not reported: a crash would make the
    /// host forget a taint the server was told of. Until the marker is written the host is
    /// tainted in memory, refuses starts and frees no slot.
    pub(super) fn pending(&self) -> Option<Taint> {
        self.taint.clone().filter(|_| self.persisted && !self.acked)
    }

    /// Notes the server's acknowledgement. One that arrives while there is no taint to
    /// report (the host is not tainted, or the marker is not written yet) acknowledges
    /// nothing, so a taint that comes later is still reported.
    pub(super) fn ack(&mut self, acked: bool) {
        self.acked |= acked && self.persisted;
    }

    /// Taints the host with `taint`, unless it is tainted already (the first stays), and
    /// writes the marker if it is not written. [`Event::Tainted`] is pushed when this call
    /// is what tainted the host, after the marker was tried and before any finding that
    /// follows from the same cause.
    ///
    /// # Errors
    ///
    /// The marker could not be written. The host stays tainted in memory.
    pub(super) fn seal(&mut self, taint: Taint, events: &mut Vec<Event>) -> io::Result<()> {
        let first = self.taint.is_none();
        if first {
            self.taint = Some(taint.clone());
        }
        let written = self.persist();
        if first {
            events.push(Event::Tainted(taint));
        }
        written
    }

    /// Writes the marker when the host is tainted and the marker does not hold it yet.
    ///
    /// # Errors
    ///
    /// The marker could not be written.
    pub(super) fn persist(&mut self) -> io::Result<()> {
        match (&self.taint, self.persisted) {
            (Some(taint), false) => {
                let written = self.root.write(taint);
                self.persisted = written.is_ok();
                written
            }
            _ => Ok(()),
        }
    }
}
