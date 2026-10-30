//! The ledger of the leases a [`CrunRuntime`](super::CrunRuntime) made: one small file
//! each, the *sidecar*, in a directory of its own beside the image store.
//!
//! `crun run` is a foreground child of the agent, so a container never outlives the
//! process that started it; what a dead process leaves is files (the lease's directories
//! and its cgroup). The ledger says which of them are this agent's to remove, and what
//! labels the run they belonged to carried. A sidecar holds the id the agent registered
//! under (the value of [`label::AGENT`]), the run id, the lease's name and every label, so
//! two agents on one host, sharing one store, never take each other's leases: a sweep
//! removes a lease only when its sidecar names this agent, and leaves every other file in
//! the directory alone.
//!
//! # Order
//!
//! The sidecar is written *before* anything it names is made, and removed *after* the last
//! of it is gone. A process that dies in between therefore leaves a sidecar with some or
//! none of its lease: the sweep removes what there is and then the sidecar. A lease with no
//! sidecar cannot have been made by this code, and is not touched.
//!
//! A sidecar is written whole to a file of its own and linked into place, so a reader sees
//! all of it or none, and an existing sidecar is never overwritten.
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

use crate::cli_runtime::container_name;
use crate::lifecycle::label;

/// The directory of the sidecars, in the store's scratch directory.
pub(super) const DIR: &str = "oci-labels";

/// What a sidecar's name ends with.
const SUFFIX: &str = ".json";

/// The longest sidecar that is read: a lease's labels are far smaller.
const MAX_BYTES: u64 = 64 * 1024;

/// The version of the sidecar's form.
const VERSION: u32 = 1;

/// One lease's sidecar.
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
}

impl Entry {
    /// The sidecar of the lease `name` of `run_id`, made by `agent`, which carries `labels`.
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

/// What reading one sidecar found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Read {
    /// There is none.
    Absent,
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

/// A lease the sweep removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// The run it was made for.
    pub run_id: String,
    /// The lease's name.
    pub name: String,
    /// The labels the run's container carried.
    pub labels: BTreeMap<String, String>,
}

/// A lease of this agent that the sweep could not remove: its sidecar stays, and the next
/// start tries again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    /// The run it was made for.
    pub run_id: String,
    /// The lease's name.
    pub name: String,
    /// What went wrong.
    pub why: String,
}

/// What a startup sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// The leases of earlier processes of this agent that were removed: the runs they
    /// belonged to are lost, and the lease server is told so by the agent holding none.
    pub removed: Vec<Removed>,
    /// The leases of this agent that could not be removed.
    pub failed: Vec<Failed>,
    /// The files of the ledger that are not this agent's, or not sidecars.
    pub left: Vec<Left>,
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
    pub(super) fn open(scratch: &Path, agent: &str) -> Result<Self, (PathBuf, io::Error)> {
        let dir = scratch.join(DIR);
        let made = fs::create_dir_all(&dir)
            .and_then(|()| fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)));
        made.map_err(|why| (dir.clone(), why))?;
        Ok(Self {
            dir,
            agent: agent.to_owned(),
        })
    }

    /// The directory of the sidecars.
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
    /// write failed.
    pub(super) fn record(&self, entry: &Entry) -> io::Result<()> {
        let path = self.path_of(&entry.name)?;
        let tmp = self.dir.join(format!(".{}.tmp", entry.name));
        let body = serde_json::to_vec(entry).map_err(io::Error::other)?;
        // A file a dead process left under this name is nobody's.
        match fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(why) if why.kind() == io::ErrorKind::NotFound => {}
            Err(why) => return Err(why),
        }
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

    /// Reads the sidecar of the lease `name`.
    pub(super) fn read(&self, name: &str) -> Read {
        let Ok(path) = self.path_of(name) else {
            return Read::Malformed(format!("{name:?} is not a lease name"));
        };
        self.load(name, &path)
    }

    /// Reads the file `path`, which must be the sidecar of the lease `stem`.
    fn load(&self, stem: &str, path: &Path) -> Read {
        // A link is refused, not followed.
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path);
        let file = match opened {
            Ok(file) => file,
            Err(why) if why.kind() == io::ErrorKind::NotFound => return Read::Absent,
            Err(why) => return Read::Malformed(format!("cannot be opened: {why}")),
        };
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
        if let Err(why) = read {
            return Read::Malformed(why.to_string());
        }
        if u64::try_from(body.len()).unwrap_or(u64::MAX) > MAX_BYTES {
            return Read::Malformed(format!("it is longer than {MAX_BYTES} bytes"));
        }
        let entry: Entry = match serde_json::from_slice(&body) {
            Ok(entry) => entry,
            Err(why) => return Read::Malformed(format!("it is not a sidecar: {why}")),
        };
        if entry.version != VERSION {
            return Read::Malformed(format!("its form is version {}", entry.version));
        }
        let named = container_name(&entry.agent, &entry.run_id).ok();
        if entry.name != stem || named.as_deref() != Some(stem) {
            return Read::Malformed(format!(
                "it names the lease {:?}, which is not its own name's",
                entry.name
            ));
        }
        if entry.agent == self.agent {
            Read::Mine(entry)
        } else {
            Read::Foreign(entry)
        }
    }

    /// Every file of the ledger: this agent's sidecars by name, and what is left alone.
    ///
    /// # Errors
    /// The directory could not be listed.
    pub(super) fn survey(&self) -> io::Result<Survey> {
        let mut names: Vec<String> = Vec::new();
        let mut survey = Survey::default();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            match entry.file_name().into_string() {
                Ok(name) => names.push(name),
                Err(_) => survey.left.push(Left::Malformed {
                    path: entry.path(),
                    why: "its name is not text".to_owned(),
                }),
            }
        }
        names.sort();
        for name in names {
            let path = self.dir.join(&name);
            let stem = name.strip_suffix(SUFFIX).filter(|stem| is_leaf(stem));
            let Some(stem) = stem else {
                survey.left.push(Left::Malformed {
                    path,
                    why: "it is not the sidecar of a lease".to_owned(),
                });
                continue;
            };
            match self.load(stem, &path) {
                Read::Mine(entry) => survey.mine.push(entry),
                Read::Foreign(entry) => survey.left.push(Left::Foreign {
                    path,
                    agent: entry.agent,
                }),
                Read::Malformed(why) => survey.left.push(Left::Malformed { path, why }),
                // Removed since the listing: nothing to report.
                Read::Absent => {}
            }
        }
        Ok(survey)
    }

    /// Removes every lease of this agent that the ledger records, with `tear_down`, which
    /// removes what one lease made. The sidecar of a lease goes once `tear_down` says it is
    /// all gone, and stays when it does not.
    ///
    /// # Errors
    /// The ledger could not be listed.
    pub(super) fn sweep(
        &self,
        mut tear_down: impl FnMut(&Entry) -> Result<(), String>,
    ) -> io::Result<SweepReport> {
        let survey = self.survey()?;
        let mut report = SweepReport {
            left: survey.left,
            ..SweepReport::default()
        };
        for entry in survey.mine {
            let gone = tear_down(&entry)
                .and_then(|()| self.forget(&entry.name).map_err(|why| why.to_string()));
            match gone {
                Ok(()) => report.removed.push(Removed {
                    run_id: entry.run_id,
                    name: entry.name,
                    labels: entry.labels,
                }),
                Err(why) => report.failed.push(Failed {
                    run_id: entry.run_id,
                    name: entry.name,
                    why,
                }),
            }
        }
        Ok(report)
    }
}
