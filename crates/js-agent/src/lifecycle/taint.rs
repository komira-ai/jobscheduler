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
//! - **The agent deletes it in one case only: a taint that cleared itself.** A taint whose
//!   reason is [`reason::RESIDUE`] or [`reason::UNCHECKED`] and whose check is one of a
//!   process, one an older agent wrote for a check that no longer taints, or of a check that
//!   could not read its input (see [`clearable`]) is re-checked
//!   while the host is idle, and clears after two clean re-checks `--taint-recheck-ms`
//!   apart and the server's acknowledgement of the clear (see "A taint that clears itself").
//!   The marker is then renamed to `taint.json.cleared-<unix seconds>`, never removed: the
//!   file is kept as an operator's clear keeps it. Every other taint ends only when an
//!   operator reimages the host, or clears it with `js-agent clear-taint`, which keeps the
//!   file aside with `taint.pinned` and the `owed-*` records (see `clear_taint`).
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
//! - **The state root must be on a disk of its own.** A job that fills a disk the state
//!   root shares makes the next write of the marker fail with `ENOSPC`; the agent is then
//!   tainted in memory only, and a crash or reboot would lose the taint. The agent refuses
//!   to start when the device number (`st_dev`) of the state root equals that of any mount
//!   root or of the scratch root ([`Local::check_filesystems`](super::Local::check_filesystems)).
//!   That is a **best-effort guard, not the guarantee**: `st_dev` does not say what shares
//!   free space (btrfs subvolumes have distinct device numbers on one pool), and the
//!   container engine's data root and the layers of its containers are not compared at
//!   all. The disk of its own is the operator's to provide.
//!
//! # The pin file
//!
//! The marker keeps the first taint only, so a non-clearable taint sealed after a clearable
//! first one pins the host in memory and nothing in the marker says so. A restart would then
//! read the restorable residue, take a baseline that already holds the new state, and clear a
//! host that holds a real change. So when the pin happens, [`PIN_FILE`] (`taint.pinned`) is
//! written beside the marker (once the marker is on disk): the marker's taint, serialised as
//! the marker is and written as atomically and durably. [`StateRoot::restored_rechecks`]
//! reads it when the agent starts: a restored residue is pinned when the pin file names the
//! same taint, field for field, or cannot be read; a pin file that names another taint is a
//! stale one of a taint an operator cleared, and is ignored. The agent never removes the pin
//! file (it never clears while pinned). A failed write is a finding
//! (`taint-write-failed`) like a marker's, the pin stands in memory, and the write is tried
//! again at the next step. An operator's clear (`js-agent clear-taint`) keeps `taint.pinned`
//! aside with `taint.json` and the `owed-*` records.
//!
//! The state root holds one more kind of file: `owed-<run id>`, the record that a job's
//! post-job checks are owed (see [`StateRoot::owe`]). It is written, synced, before the
//! first step of the job's cleanup and removed once the checks have run; one found when the
//! agent starts taints the host with the reason [`reason::CHECK_OWED`].
//!
//! **A restart or a `SIGTERM` inside the cleanup window leaves an `owed-*` record, and so
//! taints the host at the next start.** The window runs from before the first step of a
//! job's cleanup (the container's removal) until its checks have run, and it is not short:
//! it holds a container removal and the checks. An agent that is stopped, restarted or
//! killed in it, a stop on `SIGTERM` included, leaves the record behind, and the next
//! process finds it and taints the host (`check-owed`): the agent cannot tell what the job
//! left behind before the new baseline was taken. The agent never clears that taint: no
//! re-check can show that what the job left was not already part of the baseline. A clear
//! (the agent's or an operator's) **must remove the `owed-*` records as well as
//! `taint.json`** (and move `taint.pinned` aside): clearing the marker alone leaves the records, and the very next start
//! taints the host again. The agent's clear removes the records of the runs it frees from
//! `Blocked` (their checks found the taint), which are the only ones a clearable taint can
//! leave.
//!
//! # A taint that clears itself
//!
//! A host that is tainted for what a check *judged* (a process of a job's user that outlived
//! the job's container and the settle wait, or a check that could not read its input) may
//! have been tainted for a moment of the host's own: on
//! 2026-11-15 every build node was out of service for processes that were gone seconds
//! after they were seen. So such a taint clears itself, by the rules of
//! `docs/design/taint-self-clearing.md`:
//!
//! 0. A taint found in the marker at start clears only when it is a residue of a post-job
//!    check ([`restorable`]): `process-as-job-user`, or one of the codes an older agent
//!    wrote for what the checks now only note. It was judged against another process's
//!    baseline, and this process's baseline already holds what the job left, so its
//!    re-checks also apply a rule that needs no baseline: a process whose uid is in the job
//!    users' range is a finding even when the baseline holds it (the agent's own process
//!    aside); one whose parent is `init` in a control group a job can be in is only named in
//!    a `daemonised-child` note. The checks
//!    no longer find a mount, user, group, module or watched-directory change, so a re-check
//!    of an old marker of one passes unless a job's process is left, and the marker clears.
//!    A residue the pin file names is pinned too (see "The pin file"). Every other
//!    restored taint (`unchecked` of any kind, `cleanup-failed`, `check-owed`, `host-job`,
//!    `marker-unreadable`) stays until an operator moves the marker aside, as does a
//!    restored taint beside which a non-clearable one was sealed at start (owed records, a
//!    host job taken over).
//! 1. Only a taint [`clearable`] says so: reason [`reason::RESIDUE`] with the check
//!    `process-as-job-user` (the one residue the checks still make) or one of
//!    `daemonised-child`, `new-mount`, `user-changed`, `group-changed`,
//!    `kernel-module-changed`, `watch-dir-changed` (written by an older agent: what a
//!    contained job cannot cause is a note now), or reason [`reason::UNCHECKED`] with a
//!    check that could not read its input (`*-unreadable`, `watch-over-bound`, and the
//!    native leak scan's `scan-unreadable` and `scan-failed`) or
//!    `cgroup-not-found` (a crun run whose container group was not found within the settle
//!    wait while its first process ran in another group; the checks do not depend on that
//!    group). Never `cleanup-failed`, `check-owed`,
//!    `host-job`, `marker-unreadable`, `host-not-visible`
//!    (a re-check does not show that the agent now sees the host). A non-clearable taint
//!    sealed *after* a clearable one (the first stays in the marker) keeps the host from
//!    clearing for the life of the process, and, by the pin file, for every process after.
//! 2. The host is idle: no run is held but `Blocked` or `Freed` ones (their container and
//!    scratch are gone), and no container the agent could not remove.
//! 3. A full check (every check of the table, with the settle wait, against the baseline
//!    taken before the tainting job) passes, and passes again at least `--taint-recheck-ms`
//!    after. What the check notes (see [`CheckNote`](super::CheckNote)) does not fail it. The
//!    first re-check is due one interval after the agent first sees the taint
//!    clearable and idle; a re-check that finds anything, or a host that is not idle, starts
//!    the count again. A process still present at a re-check keeps the host tainted.
//! 4. The agent then reports the cleared taint (`taint_cleared` in the heartbeat) until the
//!    server acknowledges it (`taint_cleared_ack`), and stays tainted until then, so that the server and the agent
//!    agree and a restart before the acknowledgement finds the marker and starts over. On
//!    the acknowledgement it removes the `owed-*` records of its `Blocked` runs, renames the
//!    marker to `taint.json.cleared-<unix seconds>`, frees those runs and their slots, and
//!    logs the clear as a finding (`taint-cleared`) naming the taint and the two checks.
//!
//! # What the lifecycle does with it
//!
//! The first taint's detail, the one line that says what changed, is kept in memory beside
//! it ([`Lifecycle::taint_detail`](super::Lifecycle::taint_detail)) and not in the marker, so
//! a taint read from the marker at start has none. The worker prints it with the taint and the
//! health route names it.
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
use std::time::Duration;

use js_agent_wire::Taint;

use super::Event;
use super::scratch::private_dir;
use crate::integrity::code as check;

/// The name of the marker file in the state root.
pub const MARKER_FILE: &str = "taint.json";

/// The temporary file the marker is written through, renamed over [`MARKER_FILE`].
pub const MARKER_TEMP_FILE: &str = "taint.json.tmp";

/// The name of the pin file in the state root: the taint the marker held when the agent
/// sealed a taint beside it that no re-check can clear. See "The pin file" in the module
/// documentation.
pub const PIN_FILE: &str = "taint.pinned";

/// The temporary file the pin file is written through, renamed over [`PIN_FILE`].
pub const PIN_TEMP_FILE: &str = "taint.pinned.tmp";

/// The start of the name a cleared marker is kept under: `taint.json.cleared-<unix seconds>`,
/// the name an operator's clear uses.
pub const CLEARED_PREFIX: &str = "taint.json.cleared-";

/// The wait between two re-checks of a taint that may clear itself when `--taint-recheck-ms`
/// is not given: a minute, long enough to outlast a host's own churn at the end of a job and
/// short enough that a node is back in service within minutes.
pub const DEFAULT_TAINT_RECHECK: Duration = Duration::from_secs(60);

/// The most milliseconds `--taint-recheck-ms` takes: an hour.
pub const MAX_TAINT_RECHECK_MS: u64 = 3_600_000;

/// The clean re-checks in a row that clear a taint.
const CLEAN_RECHECKS: u32 = 2;

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
        self.write_named(MARKER_TEMP_FILE, MARKER_FILE, taint)
    }

    /// Writes `taint` as the pin file ([`PIN_FILE`]), the way the marker is written: the
    /// pin says the marker's taint is pinned, and holds that taint as the marker holds it.
    ///
    /// # Errors
    ///
    /// Any step failed. The pin file is then what it was before, or the new one.
    pub fn write_pin(&self, taint: &Taint) -> io::Result<()> {
        self.write_named(PIN_TEMP_FILE, PIN_FILE, taint)
    }

    /// Whether a taint read from the marker is one a restarted agent re-checks: a residue of
    /// a post-job check ([`restorable`]) that the pin file does not name. A pin file that names
    /// another taint is a stale one (of a taint an operator cleared) and says nothing; one
    /// that cannot be read, or is not a taint, pins, as a marker that cannot be read taints.
    /// This is the one decision the agent's start (`TaintState::load`) and the update hook share.
    pub fn restored_rechecks(&self, taint: &Taint) -> bool {
        restorable(taint) && !self.pins(taint)
    }

    /// Whether the pin file is there and names `taint`, or cannot be read.
    fn pins(&self, taint: &Taint) -> bool {
        let text = File::open(self.path.join(PIN_FILE)).and_then(|file| {
            let mut text = String::new();
            file.take(MARKER_MAX)
                .read_to_string(&mut text)
                .map(|_| text)
        });
        match text {
            Err(why) if why.kind() == io::ErrorKind::NotFound => false,
            Err(_) => true,
            Ok(text) => serde_json::from_str::<Taint>(&text)
                .ok()
                .is_none_or(|named| named == *taint),
        }
    }

    /// Writes `taint` to `name` through `temp`: synced, renamed, and the directory synced.
    fn write_named(&self, temp: &str, name: &str, taint: &Taint) -> io::Result<()> {
        let temp = self.path.join(temp);
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
            .and_then(|()| fs::rename(&temp, self.path.join(name)))
            .and_then(|()| self.sync())
    }
}

impl StateRoot {
    /// Keeps the marker under the name of a cleared one, `taint.json.cleared-<stamp>`,
    /// and makes the rename durable. A marker that is not there is already kept.
    ///
    /// # Errors
    ///
    /// The rename or the sync of the directory failed.
    pub fn keep_cleared(&self, stamp: u64) -> io::Result<()> {
        let kept = self.path.join(format!("{CLEARED_PREFIX}{stamp}"));
        match fs::rename(self.marker_path(), kept) {
            Ok(()) => self.sync(),
            Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(why) => Err(why),
        }
    }

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

/// The checks whose residue the agent re-checks and clears: `process-as-job-user`, which the
/// checks still make, and the codes an older agent wrote for what the checks now only note
/// (an orphan, a mount, a user, a group, a kernel module, a watched directory). A re-check of
/// one of the latter passes unless a job's process is left.
const RESIDUE_CHECKS: [&str; 7] = [
    check::PROCESS_AS_JOB_USER,
    check::DAEMONISED_CHILD,
    check::NEW_MOUNT,
    check::USER_CHANGED,
    check::GROUP_CHANGED,
    check::MODULE_CHANGED,
    check::WATCH_DIR_CHANGED,
];

/// Whether `taint` is one the agent may clear itself (see "A taint that clears itself"): a
/// residue of a post-job check ([`RESIDUE_CHECKS`]: a process of a job's user, or what an
/// older agent tainted for), a check that could not read its input, or a run whose container
/// group was not found (`cgroup-not-found`: the checks do not depend on that group, so a
/// re-check speaks for the host). Anything else is state the host really holds, or a taint a
/// re-check cannot speak for.
pub fn clearable(taint: &Taint) -> bool {
    let code = taint.check.as_deref().unwrap_or_default();
    match taint.reason.as_str() {
        reason::RESIDUE => RESIDUE_CHECKS.contains(&code),
        reason::UNCHECKED => [
            check::PROCESSES_UNREADABLE,
            check::MOUNTS_UNREADABLE,
            check::USERS_UNREADABLE,
            check::GROUPS_UNREADABLE,
            check::MODULES_UNREADABLE,
            check::WATCH_UNREADABLE,
            check::WATCH_OVER_BOUND,
            super::code::CGROUP_NOT_FOUND,
            crate::leak_scan::code::SCAN_UNREADABLE,
            crate::leak_scan::code::SCAN_FAILED,
        ]
        .contains(&code),
        _ => false,
    }
}

/// Whether a taint read from the marker at start may be re-checked: a residue of a post-job
/// check ([`RESIDUE_CHECKS`]), which a re-check judges without the baseline (see
/// [`CheckHook::run_restored`](super::CheckHook::run_restored)): it finds only a job's
/// process, so it passes when none is left, whichever of the codes the marker names. Every
/// other restored taint stays pinned: a re-check of the restart baseline cannot speak for it.
///
/// The pin file ([`PIN_FILE`]) can still pin it: see [`StateRoot::restored_rechecks`].
pub fn restorable(taint: &Taint) -> bool {
    taint.reason == reason::RESIDUE && clearable(taint)
}

/// The wait between re-checks `--taint-recheck-ms` gave, or [`DEFAULT_TAINT_RECHECK`].
///
/// # Errors
///
/// The text names the flag: the value is 0 or above [`MAX_TAINT_RECHECK_MS`].
pub fn recheck_every(given: Option<u64>) -> Result<Duration, String> {
    match given {
        None => Ok(DEFAULT_TAINT_RECHECK),
        Some(0) => Err("--taint-recheck-ms: at least 1".to_owned()),
        Some(ms) if ms > MAX_TAINT_RECHECK_MS => Err(format!(
            "--taint-recheck-ms: at most {MAX_TAINT_RECHECK_MS}"
        )),
        Some(ms) => Ok(Duration::from_millis(ms)),
    }
}

/// What the lifecycle keeps of the taint: the taint, whether the marker holds it, whether
/// the server has acknowledged the report, and how far the host is from clearing it.
pub(super) struct TaintState {
    root: StateRoot,
    taint: Option<Taint>,
    /// The line that says what changed, of the first taint, when a check's finding gave one;
    /// none for a taint read from the marker at start, and for one with no finding. In memory
    /// only: the marker holds the [`Taint`].
    detail: Option<String>,
    persisted: bool,
    acked: bool,
    /// A taint the agent never clears: one sealed beside the first, or one read from the
    /// marker at start that [`restorable`] does not allow to be re-checked. The host stays
    /// tainted whatever the re-checks say.
    pinned: bool,
    /// The pin file records the pin, or needs not (the pin was found at start): no write of
    /// it is owed.
    pin_recorded: bool,
    /// The taint was read from the marker at start and may be re-checked (see [`restorable`]):
    /// its re-checks also judge what the restart baseline cannot clear.
    restored: bool,
    /// The clean re-checks in a row.
    clean: u32,
    /// When the next re-check is due, on the lifecycle's clock; none until the taint is
    /// first seen clearable and idle.
    due: Option<u64>,
    /// The re-checks passed and the clear is reported until the server acknowledges it.
    clearing: bool,
}

impl TaintState {
    /// The state on record in `root`: tainted when a marker is there. A taint read from the
    /// marker is pinned, unless it is a residue of a post-job check ([`restorable`]) that the pin
    /// file does not name ([`StateRoot::restored_rechecks`]): that one is re-checked, with
    /// the re-check that needs no baseline, because the baseline this process took already
    /// holds what the job left.
    pub(super) fn load(root: StateRoot) -> Self {
        let taint = root.read();
        let restored = taint
            .as_ref()
            .is_some_and(|taint| root.restored_rechecks(taint));
        Self {
            persisted: taint.is_some(),
            pinned: taint.is_some() && !restored,
            pin_recorded: true,
            restored,
            taint,
            detail: None,
            root,
            acked: false,
            clean: 0,
            due: None,
            clearing: false,
        }
    }

    pub(super) fn is_tainted(&self) -> bool {
        self.taint.is_some()
    }

    /// The reason the host was tainted for, when it is.
    pub(super) fn reason(&self) -> Option<&str> {
        self.taint.as_ref().map(|taint| taint.reason.as_str())
    }

    /// The line that says what changed, of the taint the host holds, when it is known.
    pub(super) fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// What the next heartbeat reports: the taint, once the marker holds it and until it is
    /// acknowledged. A taint that is not on disk is not reported: a crash would make the
    /// host forget a taint the server was told of. Until the marker is written the host is
    /// tainted in memory, refuses starts and frees no slot.
    pub(super) fn pending(&self) -> Option<Taint> {
        let reporting = self.persisted && !self.acked && !self.clearing;
        self.taint.clone().filter(|_| reporting)
    }

    /// The taint the next heartbeat reports cleared: the taint, from the second clean
    /// re-check until the server acknowledges the clear.
    pub(super) fn pending_clear(&self) -> Option<Taint> {
        self.taint.clone().filter(|_| self.clearing)
    }

    /// Whether the agent may re-check this taint at all: it is clearable, the marker holds
    /// it, no non-clearable taint was sealed beside it, and the clear is not reported yet.
    pub(super) fn rechecks(&self) -> bool {
        let clearable = self.taint.as_ref().is_some_and(clearable);
        clearable && self.persisted && !self.pinned && !self.clearing
    }

    /// Whether the taint was read from the marker at start and is re-checked with the
    /// baseline-free rules.
    pub(super) fn restored(&self) -> bool {
        self.restored
    }

    /// The host is not idle, or a re-check found something: the count starts again.
    pub(super) fn restart_rechecks(&mut self) {
        self.clean = 0;
        self.due = None;
    }

    /// Whether a re-check is due at `now` (milliseconds on the lifecycle's clock). The first
    /// call after the count started again only sets the time of the first re-check, `every`
    /// from `now`.
    pub(super) fn recheck_due(&mut self, now: u64, every: Duration) -> bool {
        let due = *self.due.get_or_insert_with(|| after(now, every));
        now >= due
    }

    /// Notes a re-check at `now` and whether it passed. Returns whether it was the second
    /// clean one in a row, which makes the taint report its clear.
    pub(super) fn rechecked(&mut self, passed: bool, now: u64, every: Duration) -> bool {
        self.due = Some(after(now, every));
        self.clean = if passed { self.clean + 1 } else { 0 };
        self.clearing = self.clean >= CLEAN_RECHECKS;
        self.clearing
    }

    /// Ends the taint: the marker is kept as `taint.json.cleared-<stamp>` and the state is
    /// that of a host that was never tainted.
    ///
    /// # Errors
    ///
    /// The marker could not be renamed. The taint stays, and the clear is still reported.
    pub(super) fn clear(&mut self, stamp: u64) -> io::Result<()> {
        self.root.keep_cleared(stamp)?;
        self.taint = None;
        self.detail = None;
        self.persisted = false;
        self.acked = false;
        self.pinned = false;
        self.restored = false;
        self.clearing = false;
        self.restart_rechecks();
        Ok(())
    }

    /// Notes the server's acknowledgement. One that arrives while there is no taint to
    /// report (the host is not tainted, or the marker is not written yet) acknowledges
    /// nothing, so a taint that comes later is still reported.
    pub(super) fn ack(&mut self, acked: bool) {
        self.acked |= acked && self.persisted;
    }

    /// Taints the host with `taint`, unless it is tainted already (the first stays), and
    /// writes the marker if it is not written. `detail` is the line that says what changed,
    /// when there is one; the first taint keeps it. [`Event::Tainted`] is pushed, with the
    /// detail, when this call is what tainted the host, after the marker was tried and before
    /// any finding that follows from the same cause.
    ///
    /// # Errors
    ///
    /// The marker could not be written. The host stays tainted in memory.
    pub(super) fn seal(
        &mut self,
        taint: Taint,
        detail: Option<String>,
        events: &mut Vec<Event>,
    ) -> io::Result<()> {
        let first = self.taint.is_none();
        if first {
            self.taint = Some(taint.clone());
            self.detail.clone_from(&detail);
        } else if !clearable(&taint) {
            // The marker keeps the first taint, and the host now has more than it says: it
            // is not cleared by a re-check, and a clear that waits for the server stops.
            self.pinned = true;
            self.pin_recorded = false;
            self.clearing = false;
        }
        let written = self.persist();
        if first {
            events.push(Event::Tainted { taint, detail });
        }
        written
    }

    /// Writes the marker when the host is tainted and the marker does not hold it yet, and
    /// then the pin file when the host is pinned in memory and the file does not record it.
    ///
    /// # Errors
    ///
    /// The marker or the pin file could not be written (the marker's error first). The
    /// host stays tainted, and pinned if it was, in memory; the write is tried again at the
    /// next call.
    pub(super) fn persist(&mut self) -> io::Result<()> {
        let marker = match (&self.taint, self.persisted) {
            (Some(taint), false) => {
                let written = self.root.write(taint);
                self.persisted = written.is_ok();
                written
            }
            _ => Ok(()),
        };
        let pin = match (
            &self.taint,
            self.persisted && self.pinned && !self.pin_recorded,
        ) {
            (Some(taint), true) => {
                let written = self
                    .root
                    .write_pin(taint)
                    .map_err(|why| io::Error::new(why.kind(), format!("{PIN_FILE}: {why}")));
                self.pin_recorded = written.is_ok();
                written
            }
            _ => Ok(()),
        };
        marker.and(pin)
    }
}

/// `now` and `every` added, in milliseconds.
fn after(now: u64, every: Duration) -> u64 {
    now.saturating_add(u64::try_from(every.as_millis()).unwrap_or(u64::MAX))
}
