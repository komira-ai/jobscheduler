//! Where `js-launch-measure` writes its result besides standard output: appended to the
//! file `--summary-file` names (the job summary of a workflow run), which keeps what it held.

use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::Path;

/// Appends `text` to the file at `path`, made if it is missing; what it held stays before it.
///
/// # Errors
///
/// The file could not be opened or written.
pub fn append(path: &Path, text: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    file.write_all(text.as_bytes())?;
    file.flush()
}
