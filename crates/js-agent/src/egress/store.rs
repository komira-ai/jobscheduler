//! The egress store: what the agent installed on the host for each wired run, one record per
//! run in the directory `egress` of the agent's state root (`--state-root`), so the next
//! process of the agent knows what is installed for a run it adopts, and removes it when the
//! run ends.
//!
//! A record is written when a run's wiring is made ([`super::Egress::attach`]), before the run's
//! sidecar says it runs, and removed once the wiring is taken down; a process that ends leaves
//! it, with the wiring. It names the run's block of the pool (and so its interface `jsvN` and
//! its addresses), the level its job asked for (and so the firewall set its interface is in) and
//! the ports it declared (and so its forwards). The firewall rules themselves are the kernel's:
//! the store says which of them are a run's.
//!
//! Each record is the file `<run>.json` (the run's container name), mode 0600, written through
//! `.<run>.json.tmp`, synced, renamed and the directory synced, so a record is whole or absent.
//! The directory is made mode 0700 when it is not there.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use js_types::{JobNetwork, PortMap};
use serde::{Deserialize, Serialize};

/// The name of the store's directory in the state root.
pub const STORE_DIR: &str = "egress";

/// The version of the records this agent writes.
const VERSION: u32 = 1;

/// What the agent installed for one wired run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The run's block of the pool.
    pub index: u32,
    /// The level its job asked for: `allow-list` or `open`.
    pub level: JobNetwork,
    /// The ports it declared, each forwarded from the host's port.
    pub ports: Vec<PortMap>,
}

/// A record as it is on disk.
#[derive(Debug, Serialize, Deserialize)]
struct OnDisk {
    version: u32,
    index: u32,
    level: String,
    #[serde(default)]
    ports: Vec<Port>,
}

/// A declared port as it is on disk.
#[derive(Debug, Serialize, Deserialize)]
struct Port {
    host: u16,
    container: u16,
}

/// The store of an agent: the directory `egress` of its state root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The store in the state root `root`, its directory made (mode 0700) when it is not there.
    ///
    /// # Errors
    ///
    /// The directory cannot be made, or what is there under its name is not a directory (a
    /// link is not followed).
    pub fn open(root: &Path) -> io::Result<Self> {
        let dir = root.join(STORE_DIR);
        match fs::create_dir(&dir) {
            Ok(()) => {
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
                sync(root)?;
            }
            Err(why) if why.kind() == io::ErrorKind::AlreadyExists => {}
            Err(why) => return Err(why),
        }
        match fs::symlink_metadata(&dir)?.is_dir() {
            true => Ok(Self { dir }),
            false => Err(io::Error::other(format!(
                "{} is not a directory",
                dir.display()
            ))),
        }
    }

    /// The directory of the store.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether `run` can name a record.
    ///
    /// # Errors
    ///
    /// It cannot: it is empty, starts with `.` or holds anything but letters, digits, `-`,
    /// `_` and `.`.
    pub fn check(&self, run: &str) -> io::Result<()> {
        self.path(run).map(drop)
    }

    /// Writes the record of `run`, replacing one that is there.
    ///
    /// # Errors
    ///
    /// `run` is not a name a record can have, or a step failed; the record is then what it was
    /// before, or the new one, never a part of it.
    pub fn write(&self, run: &str, record: &Record) -> io::Result<()> {
        let path = self.path(run)?;
        let temp = self.dir.join(format!(".{run}.json.tmp"));
        let on_disk = OnDisk {
            version: VERSION,
            index: record.index,
            level: record.level.name().to_owned(),
            ports: record
                .ports
                .iter()
                .map(|port| Port {
                    host: port.host,
                    container: port.container,
                })
                .collect(),
        };
        let text = serde_json::to_vec(&on_disk).map_err(io::Error::other)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&text)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        sync(&self.dir)
    }
    /// Removes the record of `run`; one that is not there is not an error.
    ///
    /// # Errors
    ///
    /// The file is there and cannot be removed, or the directory cannot be synced.
    pub fn remove(&self, run: &str) -> io::Result<()> {
        match fs::remove_file(self.path(run)?) {
            Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(why) => Err(why),
            Ok(()) => sync(&self.dir),
        }
    }

    /// Every record of the store, by run, each read or why it cannot be (a file that is not a
    /// record this agent can read). Files whose name does not end in `.json`, and temporary
    /// files a crash left, are not records.
    ///
    /// # Errors
    ///
    /// The directory cannot be listed.
    pub fn records(&self) -> io::Result<Vec<(String, Result<Record, String>)>> {
        let mut found = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            let Some(run) = name.strip_suffix(".json").filter(|run| named(run)) else {
                continue;
            };
            let read = fs::read(self.dir.join(&name))
                .map_err(|why| why.to_string())
                .and_then(|bytes| parse(&bytes));
            found.push((run.to_owned(), read));
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(found)
    }

    /// The path of the record of `run`.
    fn path(&self, run: &str) -> io::Result<PathBuf> {
        match named(run) {
            true => Ok(self.dir.join(format!("{run}.json"))),
            false => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{run:?} is not a name a record can have"),
            )),
        }
    }
}

/// Whether `run` can name a record: letters, digits, `-`, `_` and `.`, not starting with `.`.
fn named(run: &str) -> bool {
    !run.is_empty()
        && !run.starts_with('.')
        && run
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The record `bytes` hold.
fn parse(bytes: &[u8]) -> Result<Record, String> {
    let on_disk: OnDisk =
        serde_json::from_slice(bytes).map_err(|why| format!("not a record: {why}"))?;
    if on_disk.version != VERSION {
        return Err(format!("a record of version {}", on_disk.version));
    }
    let level: JobNetwork = on_disk
        .level
        .parse()
        .map_err(|_| format!("the level {:?} is not one", on_disk.level))?;
    Ok(Record {
        index: on_disk.index,
        level,
        ports: on_disk
            .ports
            .into_iter()
            .map(|port| PortMap {
                host: port.host,
                container: port.container,
            })
            .collect(),
    })
}

/// Syncs the directory `dir`, so a rename or a removal in it survives a power cut.
fn sync(dir: &Path) -> io::Result<()> {
    File::open(dir).and_then(|dir| dir.sync_all())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
