//! The ledger of the leases a [`CrunRuntime`](super::CrunRuntime) made: one small file
//! each, the *sidecar*, in a directory of its own beside the image store.
//!
//! A job's `crun run` is run by a supervisor process of its own, not by the agent, so the
//! container and its supervisor outlive the agent: after a restart what is left of a
//! lease is the supervisor and the container (while the job runs), the lease's directories
//! and its cgroup. The ledger says which of them are this agent's, what labels the run
//! carried, and (from version 2) what a successor needs to take the run over: the state
//! the lease had reached, what the run booked, its wall-clock limit and start, the paths
//! of the lease and of the exit record, and the identity of the supervisor. A sidecar holds
//! the id the agent registered under (the value of [`label::AGENT`]), the run id, the
//! lease's name and every label, so two agents on one host, sharing one store, never take
//! each other's leases: a start-up pass takes over or removes a lease only when its sidecar
//! names this agent, and leaves every other file in the directory alone.
//!
//! # Order
//!
//! The sidecar is written, in the state [`Phase::Starting`], *before* anything it names is
//! made, and rewritten in the state [`Phase::Running`] (with the start and the supervisor)
//! once the supervisor answered. It is removed *after* the last of the lease is gone. A
//! process that dies in between leaves a sidecar with some or none of its lease: the next
//! start removes what there is and then the sidecar. A lease with no sidecar cannot have
//! been made by this code; the start-up pass removes one that carries this agent's name
//! prefix.
//!
//! A sidecar is first written whole to a file of its own and linked into place, so a reader
//! sees all of it or none, and a sidecar that exists is never recorded over. A rewrite
//! writes a whole new file and renames it over, so a reader sees the old sidecar or the new
//! one.
//!
//! # What is left alone
//!
//! A file that is not a well-formed sidecar of the name it carries (not a regular file,
//! too large, not the JSON of an [`Entry`], a lease name that is not the one its agent and
//! run id give) is [`Left::Malformed`]; a well-formed one of another agent is
//! [`Left::Foreign`]. Neither is read further, removed, or followed, and both are reported.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use js_contain::oci::SupervisorId;

use crate::cli_runtime::{NAME_PREFIX, agent_tag, container_name};
use crate::lifecycle::label;

/// The directory of the sidecars, in the store's scratch directory.
pub(super) const DIR: &str = "oci-labels";

/// What a sidecar's name ends with.
const SUFFIX: &str = ".json";

/// The longest sidecar that is read: a lease's labels are far smaller.
const MAX_BYTES: u64 = 64 * 1024;

/// The version of the sidecar's form: version 2 adds the fields of a run that a successor
/// can take over ([`Phase`], [`Booked`], [`Supervisor`] and the paths of the lease).
pub(super) const VERSION: u32 = 2;

/// The oldest form that is still read: version 1 has none of the version 2 fields, and its
/// lease is removed, not taken over.
const OLDEST: u32 = 1;

/// How far a lease had come when its sidecar was last written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The lease is being made, or is made and its container not started.
    Starting,
    /// The supervisor of the container answered: the run runs, or ran.
    Running,
}

/// What a run booked, as the labels say it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Booked {
    /// Thousandths of a CPU core.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
    /// Bytes of scratch disk; none when the run named none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    /// The GPU indices.
    #[serde(default)]
    pub gpus: Vec<u32>,
    /// The slot the run holds.
    pub slot: u32,
}

/// The supervisor process of a run: its pid and its start time ([`SupervisorId`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Supervisor {
    /// The process id.
    pub pid: u32,
    /// The start time in clock ticks since boot.
    pub start: u64,
}

impl Supervisor {
    /// The supervisor `id` names.
    pub(super) fn of(id: SupervisorId) -> Self {
        Self {
            pid: id.pid,
            start: id.start,
        }
    }

    /// The identity this record names.
    pub(super) fn id(&self) -> SupervisorId {
        SupervisorId {
            pid: self.pid,
            start: self.start,
        }
    }
}

/// One lease's sidecar. The fields after `labels` are those of version 2: a sidecar of
/// version 1 has none of them, and one of version 2 written before the lease's container
/// started has no `started_wall_ms` and no `supervisor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// The form of this file; a sidecar of another is not read.
    pub version: u32,
    /// The id the agent that made the lease registered under.
    pub agent: String,
    /// The run's id.
    pub run_id: String,
    /// The lease's name: its directory and cgroup are named so, and so is this file.
    pub name: String,
    /// The labels of the container, the agent's own among them.
    pub labels: BTreeMap<String, String>,
    /// How far the lease had come.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Phase>,
    /// The lease id of the run (the allocation id of a job start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation: Option<String>,
    /// What the run booked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub booked: Option<Booked>,
    /// When the container started, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_wall_ms: Option<u64>,
    /// The wall-time limit of the run in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
    /// The lease's directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_dir: Option<PathBuf>,
    /// The lease's cgroup directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<PathBuf>,
    /// The run's scratch directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch: Option<PathBuf>,
    /// The file the supervisor writes the container's end in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_record: Option<PathBuf>,
    /// The digest of the run's image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_digest: Option<String>,
    /// The supervisor process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<Supervisor>,
}

impl Entry {
    /// The sidecar of the lease `name` of `run_id`, made by `agent`, which carries `labels`,
    /// in the state [`Phase::Starting`] with none of the other version 2 fields.
    pub(super) fn new(
        agent: &str,
        run_id: &str,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Self {
        let mut labels = labels.clone();
        labels.insert(label::AGENT.to_owned(), agent.to_owned());
        Self {
            version: VERSION,
            agent: agent.to_owned(),
            run_id: run_id.to_owned(),
            name: name.to_owned(),
            labels,
            state: Some(Phase::Starting),
            allocation: None,
            booked: None,
            started_wall_ms: None,
            wall_ms: None,
            lease_dir: None,
            cgroup: None,
            scratch: None,
            exit_record: None,
            image_digest: None,
            supervisor: None,
        }
    }

    /// This sidecar once the container started at `started_wall_ms` under `supervisor`.
    pub(super) fn running(&self, started_wall_ms: u64, supervisor: Supervisor) -> Self {
        Self {
            state: Some(Phase::Running),
            started_wall_ms: Some(started_wall_ms),
            supervisor: Some(supervisor),
            ..self.clone()
        }
    }
}

/// A file of the ledger that was left alone, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Left {
    /// Not a well-formed sidecar of the name it carries.
    Malformed {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        why: String,
    },
    /// A well-formed sidecar of another agent.
    Foreign {
        /// The file.
        path: PathBuf,
        /// The id of the agent it names.
        agent: String,
    },
}

/// Why the ledger's directory could not be made or listed.
#[derive(Debug, thiserror::Error)]
#[error("{}: {why}", .path.display())]
pub struct LedgerError {
    /// The directory.
    pub path: PathBuf,
    /// The reason.
    pub why: io::Error,
}

/// The error of a failed call on the directory `dir`, named. Not generic over what the call
/// returns, so that it is one function whatever is called.
fn at(dir: &Path) -> impl Fn(io::Error) -> LedgerError + '_ {
    |why| LedgerError {
        path: dir.to_owned(),
        why,
    }
}

/// What reading one sidecar found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Read {
    /// This agent's.
    Mine(Entry),
    /// Another agent's.
    Foreign(Entry),
    /// Not a sidecar.
    Malformed(String),
}

/// The sidecars found: this agent's, and the files left alone.
#[derive(Debug, Default)]
pub(super) struct Survey {
    pub(super) mine: Vec<Entry>,
    pub(super) left: Vec<Left>,
}

/// Why the start-up pass removed a lease of this agent instead of taking it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Its sidecar is of version 1, which records nothing a successor could take over.
    OldForm,
    /// Its container never started (its sidecar is not in the state running): no job ran.
    NeverRan,
    /// Its sidecar is in the state running, but a check of it failed; the text says which.
    Inconsistent(String),
    /// A lease of this agent's name prefix that has no sidecar at all: a directory beside
    /// the runs or a cgroup nothing records.
    NoRecord,
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OldForm => f.write_str("its record is of an old form"),
            Self::NeverRan => f.write_str("its container never started"),
            Self::Inconsistent(why) => write!(f, "its record does not hold: {why}"),
            Self::NoRecord => f.write_str("nothing records it"),
        }
    }
}

/// A lease the start-up pass removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// The run it was made for.
    pub run_id: String,
    /// The lease's name.
    pub name: String,
    /// The labels the run's container carried (none for a lease with no sidecar).
    pub labels: BTreeMap<String, String>,
    /// Why it was removed and not taken over.
    pub reason: Reason,
}

/// A lease of this agent that the start-up pass could not remove: its sidecar stays, and the
/// next start tries again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    /// The run it was made for.
    pub run_id: String,
    /// The lease's name.
    pub name: String,
    /// What went wrong.
    pub why: String,
}

/// What the start-up pass over the ledger did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// The runs of earlier processes of this agent that were taken over: they run on (or
    /// ended and wait to be reported), and the runtime lists them.
    pub adopted: Vec<String>,
    /// The leases of earlier processes of this agent that were removed: the runs they
    /// belonged to are lost, and the lease server is told so by the agent holding none.
    pub removed: Vec<Removed>,
    /// The leases of this agent that could not be removed.
    pub failed: Vec<Failed>,
    /// The files of the ledger that are not this agent's, or not sidecars.
    pub left: Vec<Left>,
}

/// What [`Ledger::sweep`] did, and what it kept: the report, and the value `judge` made for
/// each lease it kept.
#[derive(Debug)]
pub(super) struct Swept<T> {
    pub(super) report: SweepReport,
    pub(super) adopted: Vec<T>,
}

/// Whether `name` is one path component of the characters a lease's name has.
fn is_leaf(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 255
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The ledger of one agent.
#[derive(Debug)]
pub(super) struct Ledger {
    dir: PathBuf,
    agent: String,
}

impl Ledger {
    /// The ledger of `agent` in `scratch`, made `0700` if it is not there.
    pub(super) fn open(scratch: &Path, agent: &str) -> Result<Self, LedgerError> {
        let dir = scratch.join(DIR);
        let made = fs::create_dir_all(&dir)
            .and_then(|()| fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)));
        made.map_err(at(&dir))?;
        Ok(Self {
            dir,
            agent: agent.to_owned(),
        })
    }

    /// The directory of the sidecars.
    #[cfg(test)]
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The sidecar of the lease `name`.
    fn path_of(&self, name: &str) -> io::Result<PathBuf> {
        if is_leaf(name) {
            Ok(self.dir.join(format!("{name}{SUFFIX}")))
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name:?} is not a lease name"),
            ))
        }
    }

    /// Writes `entry`, whole or not at all, and never over a sidecar that is there.
    ///
    /// # Errors
    /// The name is not a lease's, a sidecar of that name exists (`AlreadyExists`), or the
    /// write failed. A sidecar that could not be made durable is removed again.
    pub(super) fn record(&self, entry: &Entry) -> io::Result<()> {
        self.record_synced(entry, Self::sync)
    }

    /// [`Ledger::record`] with the making of the directory durable given, so that a test
    /// can fail it.
    pub(super) fn record_synced(
        &self,
        entry: &Entry,
        sync: fn(&Self) -> io::Result<()>,
    ) -> io::Result<()> {
        let path = self.path_of(&entry.name)?;
        let tmp = self.dir.join(format!(".{}.tmp", entry.name));
        let body = serde_json::to_vec(entry).map_err(io::Error::other)?;
        // A file a dead process left under this name is nobody's; one that stays makes
        // the `create_new` below fail.
        let _ = fs::remove_file(&tmp);
        let written = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut file| {
                file.write_all(&body)?;
                file.sync_all()
            })
            // Fails when the sidecar exists: one name is never recorded twice.
            .and_then(|()| fs::hard_link(&tmp, &path));
        let _ = fs::remove_file(&tmp);
        written?;
        // A sidecar that is not durable is not kept: the call fails, and the name can be
        // recorded again.
        let synced = sync(self);
        if synced.is_err() {
            let _ = fs::remove_file(&path);
        }
        synced
    }

    /// Replaces the sidecar of `entry.name` with `entry`, whole or not at all: a new file is
    /// written, made durable and renamed over the old one, and the directory is made durable.
    ///
    /// # Errors
    /// The name is not a lease's, or the write failed (the old sidecar is then as it was).
    pub(super) fn rewrite(&self, entry: &Entry) -> io::Result<()> {
        let path = self.path_of(&entry.name)?;
        let tmp = self.dir.join(format!(".{}.tmp", entry.name));
        let body = serde_json::to_vec(entry).map_err(io::Error::other)?;
        let _ = fs::remove_file(&tmp);
        let written = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut file| {
                file.write_all(&body)?;
                file.sync_all()
            })
            .and_then(|()| fs::rename(&tmp, &path));
        // A file that was not renamed over is nobody's.
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written?;
        self.sync()
    }

    /// Removes the sidecar of `name`; one that is not there is removed already.
    ///
    /// # Errors
    /// The name is not a lease's, or the file could not be removed.
    pub(super) fn forget(&self, name: &str) -> io::Result<()> {
        match fs::remove_file(self.path_of(name)?) {
            Ok(()) => {}
            Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(why) => return Err(why),
        }
        self.sync()
    }

    /// Makes the directory's entries durable.
    fn sync(&self) -> io::Result<()> {
        File::open(&self.dir)?.sync_all()
    }

    /// Reads the sidecar of the lease `name`; `None` if there is no file of that name.
    pub(super) fn read(&self, name: &str) -> Option<Read> {
        let Ok(path) = self.path_of(name) else {
            return Some(Read::Malformed(format!("{name:?} is not a lease name")));
        };
        // A link that points nowhere is a file of that name, and malformed.
        if fs::symlink_metadata(&path).is_err() {
            return None;
        }
        Some(self.classify(self.load(name, &path)))
    }

    /// Whose a sidecar that was read is.
    fn classify(&self, loaded: Result<Entry, String>) -> Read {
        match loaded {
            Ok(entry) if entry.agent == self.agent => Read::Mine(entry),
            Ok(entry) => Read::Foreign(entry),
            Err(why) => Read::Malformed(why),
        }
    }

    /// Reads the file `path`, which must be the sidecar of the lease `stem`.
    fn load(&self, stem: &str, path: &Path) -> Result<Entry, String> {
        // A link is refused, not followed.
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path);
        let file = opened.map_err(|why| format!("cannot be opened: {why}"))?;
        let mut body = Vec::new();
        let read = file
            .metadata()
            .and_then(|meta| {
                if meta.is_file() {
                    Ok(())
                } else {
                    Err(io::Error::other("it is not a regular file"))
                }
            })
            .and_then(|()| file.take(MAX_BYTES + 1).read_to_end(&mut body));
        read.map_err(|why| why.to_string())?;
        if u64::try_from(body.len()).unwrap_or(u64::MAX) > MAX_BYTES {
            return Err(format!("it is longer than {MAX_BYTES} bytes"));
        }
        let entry: Entry =
            serde_json::from_slice(&body).map_err(|why| format!("it is not a sidecar: {why}"))?;
        if !(OLDEST..=VERSION).contains(&entry.version) {
            return Err(format!("its form is version {}", entry.version));
        }
        let named = container_name(&entry.agent, &entry.run_id).ok();
        if entry.name != stem || named.as_deref() != Some(stem) {
            return Err(format!(
                "it names the lease {:?}, which is not its own name's",
                entry.name
            ));
        }
        Ok(entry)
    }

    /// Every file of the ledger: this agent's sidecars by name, and what is left alone.
    ///
    /// # Errors
    /// The directory could not be listed.
    pub(super) fn survey(&self) -> Result<Survey, LedgerError> {
        let mut names: Vec<String> = Vec::new();
        let mut survey = Survey::default();
        for entry in fs::read_dir(&self.dir).map_err(at(&self.dir))? {
            let entry = entry.map_err(at(&self.dir))?;
            match entry.file_name().into_string() {
                Ok(name) => names.push(name),
                Err(_) => survey.left.push(Left::Malformed {
                    path: entry.path(),
                    why: "its name is not text".to_owned(),
                }),
            }
        }
        names.sort();
        let temp = format!(".{NAME_PREFIX}{}-", agent_tag(&self.agent));
        for name in names {
            let path = self.dir.join(&name);
            // A file this agent was writing when its process died; the lease it was for
            // was not made (the sidecar is written first), so nothing names it.
            let rest = name.strip_prefix('.').and_then(|n| n.strip_suffix(".tmp"));
            if rest.is_some_and(|rest| name.starts_with(&temp) && is_leaf(rest))
                && fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file())
            {
                let _ = fs::remove_file(&path);
                continue;
            }
            let stem = name.strip_suffix(SUFFIX).filter(|stem| is_leaf(stem));
            let Some(stem) = stem else {
                survey.left.push(Left::Malformed {
                    path,
                    why: "it is not the sidecar of a lease".to_owned(),
                });
                continue;
            };
            match self.classify(self.load(stem, &path)) {
                Read::Mine(entry) => survey.mine.push(entry),
                Read::Foreign(entry) => survey.left.push(Left::Foreign {
                    path,
                    agent: entry.agent,
                }),
                Read::Malformed(why) => survey.left.push(Left::Malformed { path, why }),
            }
        }
        Ok(survey)
    }

    /// Removes the lease of `entry` with `tear_down` (which removes what the lease made) and
    /// then its sidecar. The sidecar stays when `tear_down` fails.
    ///
    /// # Errors
    /// What failed, as text.
    pub(super) fn discard(
        &self,
        entry: &Entry,
        tear_down: impl FnOnce(&Entry) -> Result<(), String>,
    ) -> Result<(), String> {
        tear_down(entry)?;
        self.forget(&entry.name)
            .map_err(|why| format!("remove the sidecar: {why}"))
    }

    /// Goes over every lease of this agent that the ledger records: `judge` says whether the
    /// lease is taken over (the value it makes is kept in the result) or why it is not, and
    /// the lease that is not is removed with `tear_down` and then its sidecar. The sidecar of
    /// a lease that cannot be removed stays.
    ///
    /// # Errors
    /// The ledger could not be listed.
    pub(super) fn sweep<T>(
        &self,
        mut judge: impl FnMut(&Entry) -> Result<T, Reason>,
        mut tear_down: impl FnMut(&Entry) -> Result<(), String>,
    ) -> Result<Swept<T>, LedgerError> {
        let survey = self.survey()?;
        let mut swept = Swept {
            report: SweepReport {
                left: survey.left,
                ..SweepReport::default()
            },
            adopted: Vec::new(),
        };
        for entry in survey.mine {
            let reason = match judge(&entry) {
                Ok(kept) => {
                    swept.report.adopted.push(entry.run_id);
                    swept.adopted.push(kept);
                    continue;
                }
                Err(reason) => reason,
            };
            match self.discard(&entry, &mut tear_down) {
                Ok(()) => swept.report.removed.push(Removed {
                    run_id: entry.run_id,
                    name: entry.name,
                    labels: entry.labels,
                    reason,
                }),
                Err(why) => swept.report.failed.push(Failed {
                    run_id: entry.run_id,
                    name: entry.name,
                    why,
                }),
            }
        }
        Ok(swept)
    }
}
