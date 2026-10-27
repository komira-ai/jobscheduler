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
//!
//! # What the lifecycle does with it
//!
//! A tainted agent starts no job (every `Start` is answered `failed_to_start` with the code
//! `host-tainted`) and lets the jobs it runs finish. [`Lifecycle::pending_taint`] is what
//! the next heartbeat's `taint` field carries until the server acknowledges it
//! (`taint_ack`); the acknowledgement stops the report and keeps the marker. A marker that
//! cannot be written does not clear the taint: the agent is tainted in memory, reports it,
//! refuses new starts, frees no slot, and writes the marker again at every step.
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
            .and_then(|()| File::open(&self.path))
            .and_then(|dir| dir.sync_all())
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

    /// What the next heartbeat reports: the taint, until it is acknowledged.
    pub(super) fn pending(&self) -> Option<Taint> {
        self.taint.clone().filter(|_| !self.acked)
    }

    /// Notes the server's acknowledgement. One that arrives while the host is not tainted
    /// acknowledges nothing, so a taint that comes later is still reported.
    pub(super) fn ack(&mut self, acked: bool) {
        self.acked |= acked && self.taint.is_some();
    }

    /// Taints the host with `taint`, unless it is tainted already (the first stays), and
    /// writes the marker if it is not written. [`Event::Tainted`] is pushed when this call
    /// is what tainted the host, before the marker is written.
    ///
    /// # Errors
    ///
    /// The marker could not be written. The host stays tainted in memory.
    pub(super) fn seal(&mut self, taint: Taint, events: &mut Vec<Event>) -> io::Result<()> {
        if self.taint.is_none() {
            events.push(Event::Tainted(taint.clone()));
            self.taint = Some(taint);
        }
        self.persist()
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
