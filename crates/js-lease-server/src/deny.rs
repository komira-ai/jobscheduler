//! The hosts the agent routes refuse (`--agent-deny-file`): one host id per line.
//!
//! Blank lines and lines starting with `#` are skipped, and a line may have spaces around its
//! host id. The file is read at start, where a line that is not a host id stops the start, and
//! again at every tick, where a file that cannot be read or is not valid keeps the set in
//! force. A refusal names the line's number and never anything of its text, so a secret
//! pasted into the file by mistake is not logged. The file holds no secret and is not
//! required to be owner-only.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

use js_types::HostId;

use crate::error::StartupError;

/// The flag the refusals name.
const FLAG: &str = "--agent-deny-file";

/// The most bytes of the file that are read; a larger file is refused.
const MAX_BYTES: u64 = 1 << 20;

/// The code of the 403 a request for a denied host is answered with.
pub(crate) const HOST_DENIED: &str = "host-denied";

/// The denied hosts, as of the last good read of the file; none without `--agent-deny-file`.
pub(crate) struct DenyList {
    file: Option<PathBuf>,
    hosts: RwLock<BTreeSet<HostId>>,
}

impl DenyList {
    /// A list that denies no host, with no file to read.
    pub(crate) fn none() -> Self {
        Self {
            file: None,
            hosts: RwLock::default(),
        }
    }

    /// The list in the file at `file`, which must be readable and valid.
    pub(crate) fn open(file: PathBuf) -> Result<Self, StartupError> {
        let hosts = read(&file)?;
        Ok(Self {
            file: Some(file),
            hosts: RwLock::new(hosts),
        })
    }

    /// Reads the file again and denies what it names from now on, in place of what was
    /// denied; a file that cannot be read, or is not valid, changes nothing. Without a file it
    /// does nothing. A blocking call.
    pub(crate) fn reload(&self) -> Result<(), StartupError> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let fresh = read(file)?;
        *self.hosts.write().unwrap_or_else(PoisonError::into_inner) = fresh;
        Ok(())
    }

    /// Whether `host` is denied.
    pub(crate) fn denies(&self, host: &HostId) -> bool {
        let hosts = self.hosts.read().unwrap_or_else(PoisonError::into_inner);
        hosts.contains(host)
    }

    /// Whether the host named `text` is denied; a text that is no host id names none.
    pub(crate) fn denies_text(&self, text: &str) -> bool {
        HostId::new(text).is_ok_and(|host| self.denies(&host))
    }
}

/// The hosts named by the file at `path`.
fn read(path: &Path) -> Result<BTreeSet<HostId>, StartupError> {
    let refuse = |why: String| StartupError::new(FLAG, format!("{}: {why}", path.display()));
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|err| refuse(err.to_string()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_BYTES {
        return Err(refuse(format!("is larger than {MAX_BYTES} bytes")));
    }
    let text = String::from_utf8(bytes).map_err(|_| refuse("is not text".to_owned()))?;
    let mut hosts = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let host =
            HostId::new(line).map_err(|_| refuse(format!("line {}: not a host id", index + 1)))?;
        hosts.insert(host);
    }
    Ok(hosts)
}

#[cfg(test)]
#[path = "deny_tests.rs"]
mod tests;
