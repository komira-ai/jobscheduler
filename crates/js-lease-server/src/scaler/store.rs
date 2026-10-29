//! The scaler's records file: `<state file>.scaler`.
//!
//! The file holds all of [`Records`] ([`Records::encode`] is its format: JSON, versioned),
//! and the shell writes all of it, as one file, before it runs the first call a step
//! decided. It is written as the file of host bindings is ([`crate::agent_state::replace`]):
//! the whole text goes to a new file beside it that only the owner can use, the new file is
//! fsynced, renamed over the old one, and the directory is fsynced, so a reader sees the
//! old records or the new ones and never a mixture, whenever the process stops. A reader
//! refuses a file anyone else can use, one of another version and one that is not records:
//! a start with no records would double-launch.

use std::io;
use std::path::PathBuf;

use js_scaler::Records;

use crate::agent_state::{read_if_present, replace};
use crate::error::StartupError;

/// The flag the refusals name: the file lives beside the lease file.
const FLAG: &str = "--state-file";

/// Where the scaler's records are kept.
#[derive(Debug, Clone)]
pub struct RecordsFile {
    path: PathBuf,
}

impl RecordsFile {
    /// The file at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The records in the file; none when there is no file (the first start).
    ///
    /// A live pool does not start on this: it starts on [`RecordsFile::load_existing`]
    /// unless the operator said it is the first start.
    ///
    /// # Errors
    ///
    /// [`StartupError`] when the file is not owner-only, cannot be read, is not text, is of
    /// another version, or is not records. The scaler does not start on a file it cannot
    /// read.
    pub fn load(&self) -> Result<Records, StartupError> {
        Ok(self.read()?.unwrap_or_default())
    }

    /// The records in the file, which must be there.
    ///
    /// A start with no records is a start that has forgotten every machine in flight and
    /// the spend of the day, so a live pool that was not told this is its first start does
    /// not take a missing file for one.
    ///
    /// # Errors
    ///
    /// [`StartupError`] as for [`RecordsFile::load`], and when there is no file; the
    /// refusal names the file and the remedy.
    pub fn load_existing(&self) -> Result<Records, StartupError> {
        match self.read()? {
            Some(records) => Ok(records),
            None => Err(self.missing()),
        }
    }

    fn read(&self) -> Result<Option<Records>, StartupError> {
        let Some(bytes) = read_if_present(&self.path)? else {
            return Ok(None);
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return Err(self.refused("it is not text")),
        };
        match Records::decode(&text) {
            Ok(records) => Ok(Some(records)),
            Err(err) => Err(self.refused(err)),
        }
    }

    fn missing(&self) -> StartupError {
        let named = format!(
            "{} is missing, and a pool with launch = \"live\" does not start without its scaler records: it would launch again for machines that may already exist and forget the day's spend. Restore the file; if this is the first start of the pool, start once with --boot initialise (boot = \"initialise\" in the file)",
            self.path.display()
        );
        StartupError::new(FLAG, named)
    }

    /// Replaces the file with `records`, all of them.
    ///
    /// # Errors
    ///
    /// The I/O error when the file could not be written; the old file is then still there.
    pub fn save(&self, records: &Records) -> io::Result<()> {
        let text = records.encode().map_err(io::Error::other)?;
        replace(&self.path, text.as_bytes())
    }

    fn refused(&self, why: impl std::fmt::Display) -> StartupError {
        let named = format!(
            "{} is not a valid scaler records file: {why}",
            self.path.display()
        );
        StartupError::new(FLAG, named)
    }
}
