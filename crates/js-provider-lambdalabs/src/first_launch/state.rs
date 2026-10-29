//! The state file: what a later `--teardown-only` needs to find the instance again.
//!
//! Plain `key=value` lines, written owner-only. It is written before the launch is sent
//! (with no instance yet) and again when the launch answers, so a crash at any point
//! leaves enough to look for the instance: its acquire id. Each step ends with an fsync of
//! the directory that holds the file, so that the record of a live instance survives a
//! crash right after the step (a file's own fsync does not make its directory entry
//! durable).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use js_elastic::{AcquireId, HostRef};

use super::DirSync;
use crate::model::InstanceId;

/// Makes the directory entries of `dir` durable. The seam of the runner takes this as a
/// function so that a test can count the calls.
///
/// # Errors
///
/// The error of opening or syncing the directory.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// The directory that holds `path`; the current directory for a bare file name.
fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// A state file's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct State {
    pub acquire: AcquireId,
    /// Seconds since the Unix epoch when the launch was sent.
    pub launched_unix: u64,
    pub key_name: String,
    pub instance_type: String,
    pub region: String,
    pub price_cents_per_hour: u64,
    /// The instances the launch answered with; empty until it does.
    pub instances: Vec<HostRef>,
}

/// Why a state file could not be used.
#[derive(Debug, thiserror::Error)]
pub(super) enum StateError {
    #[error("{path}: {why}")]
    Io { path: PathBuf, why: String },
    #[error("{path}: is not a state file of this runner")]
    Format { path: PathBuf },
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> StateError + '_ {
    move |err| StateError::Io {
        path: path.to_owned(),
        why: err.kind().to_string(),
    }
}

impl State {
    fn text(&self) -> String {
        let ids: Vec<&str> = self.instances.iter().map(HostRef::as_str).collect();
        format!(
            "first-launch-state=1\nacquire={}\nlaunched_unix={}\nkey_name={}\ninstance_type={}\nregion={}\nprice_cents_per_hour={}\ninstances={}\n",
            self.acquire,
            self.launched_unix,
            self.key_name,
            self.instance_type,
            self.region,
            self.price_cents_per_hour,
            ids.join(",")
        )
    }

    /// Creates `path`, which must not exist, and syncs its directory.
    pub(super) fn create(&self, path: &Path, sync: &DirSync<'_>) -> Result<(), StateError> {
        write_new(path, &self.text())?;
        sync(parent_of(path)).map_err(io(path))
    }

    /// Replaces the content of `path` in one step: a staging file next to it, made new
    /// (any stale one removed first) and owner-only, renamed over `path`, then the
    /// directory synced.
    pub(super) fn replace(&self, path: &Path, sync: &DirSync<'_>) -> Result<(), StateError> {
        let mut name = path.as_os_str().to_owned();
        name.push(".new");
        let staging = PathBuf::from(name);
        let _ = fs::remove_file(&staging);
        write_new(&staging, &self.text())?;
        fs::rename(&staging, path).map_err(io(path))?;
        sync(parent_of(path)).map_err(io(path))
    }

    /// Reads `path`.
    pub(super) fn read(path: &Path) -> Result<Self, StateError> {
        let text = fs::read_to_string(path).map_err(io(path))?;
        let bad = || StateError::Format {
            path: path.to_owned(),
        };
        let mut lines = text.lines();
        if lines.next() != Some("first-launch-state=1") {
            return Err(bad());
        }
        let field = |lines: &mut std::str::Lines<'_>, key: &str| -> Result<String, StateError> {
            let line = lines.next().ok_or_else(bad)?;
            let (name, value) = line.split_once('=').ok_or_else(bad)?;
            if name == key {
                Ok(value.to_owned())
            } else {
                Err(bad())
            }
        };
        let acquire = AcquireId::new(&field(&mut lines, "acquire")?).map_err(|_| bad())?;
        let launched_unix = field(&mut lines, "launched_unix")?
            .parse()
            .map_err(|_| bad())?;
        let key_name = field(&mut lines, "key_name")?;
        let instance_type = field(&mut lines, "instance_type")?;
        let region = field(&mut lines, "region")?;
        let price_cents_per_hour = field(&mut lines, "price_cents_per_hour")?
            .parse()
            .map_err(|_| bad())?;
        let ids = field(&mut lines, "instances")?;
        let instances = ids
            .split(',')
            .filter(|id| !id.is_empty())
            .map(|id| {
                InstanceId::new(id)
                    .map(|valid| valid.host_ref())
                    .map_err(|_| bad())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            acquire,
            launched_unix,
            key_name,
            instance_type,
            region,
            price_cents_per_hour,
            instances,
        })
    }
}

fn write_new(path: &Path, text: &str) -> Result<(), StateError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io(path))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(io(path))
}
