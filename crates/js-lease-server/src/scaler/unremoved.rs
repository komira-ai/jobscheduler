//! The hosts the registry refused to remove: `<state file>.scaler.unremoved`.
//!
//! The scaler's shell keeps the hosts of machines that are confirmed gone and that the
//! registry still holds (a lease is booked on them) and tries to remove them at every
//! tick. This file keeps that set across a restart, so that the first tick after it tries
//! again; without it such a host leaves the registry only through the registry's own
//! `forget_lost`, 24 hours after it was lost, and holds its slot until then.
//!
//! It is a file of the shell's, apart from the scaler's records ([`super::RecordsFile`]),
//! whose format is the scaler's. The text is one JSON object, `{"version":1,"hosts":[...]}`
//! with the host ids in order. It is written whole, as the records are: to a new file
//! beside it that only the owner can use, fsynced, renamed over the old one, and the
//! directory fsynced. A reader refuses a file anyone else can use, one of another version
//! and one that is not this list: reading a damaged file as an empty list would drop the
//! retries without a word. No file is an empty list.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::agent_state::{read_if_present, replace};
use crate::error::StartupError;

/// The flag the refusals name: the file lives beside the lease file.
const FLAG: &str = "--state-file";

/// The version of the file. A reader refuses any other.
const VERSION: u64 = 1;

/// The file's fields, once its version is known to be [`VERSION`].
#[derive(Deserialize)]
struct Listed {
    hosts: BTreeSet<String>,
}

/// Where the hosts the registry refused to remove are kept.
#[derive(Debug, Clone)]
pub struct UnremovedFile {
    path: PathBuf,
}

impl UnremovedFile {
    /// The file at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The hosts in the file; none when there is no file.
    ///
    /// # Errors
    ///
    /// [`StartupError`] naming the file when it is not owner-only, cannot be read, is not
    /// text, is of another version, or is not a list of hosts. The scaler does not start
    /// on a file it cannot read.
    pub fn load(&self) -> Result<BTreeSet<String>, StartupError> {
        let Some(bytes) = read_if_present(&self.path)? else {
            return Ok(BTreeSet::new());
        };
        let text = String::from_utf8(bytes).map_err(|_| "it is not text".to_owned());
        let hosts = text.and_then(|text| decode(&text));
        hosts.map_err(|why| {
            let named = format!(
                "{} is not a valid scaler unremoved-hosts file: {why}",
                self.path.display()
            );
            StartupError::new(FLAG, named)
        })
    }

    /// Replaces the file with `hosts`, all of them.
    ///
    /// # Errors
    ///
    /// The I/O error when the file could not be written; the old file is then still there.
    pub fn save(&self, hosts: &BTreeSet<String>) -> io::Result<()> {
        replace(&self.path, encode(hosts).as_bytes())
    }
}

/// The text of a file that lists `hosts`.
fn encode(hosts: &BTreeSet<String>) -> String {
    let listed = Value::from_iter(hosts.iter().map(String::as_str));
    format!("{{\"version\":{VERSION},\"hosts\":{listed}}}")
}

/// The hosts a file's text lists, or what is wrong with it.
fn decode(text: &str) -> Result<BTreeSet<String>, String> {
    let value: Value = serde_json::from_str(text).map_err(|err| err.to_string())?;
    let found = value.get("version").and_then(Value::as_u64);
    if found != Some(VERSION) {
        return Err(format!("version {found:?}, this reader reads {VERSION}"));
    }
    let listed: Listed = serde_json::from_value(value).map_err(|err| err.to_string())?;
    Ok(listed.hosts)
}
