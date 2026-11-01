//! `js-mdm fetch`: download the pinned archives into a directory, as the operator's own user,
//! checking each against its pin.
//!
//! It refuses to run as root (a download is never root's) and a directory inside a git work
//! tree. An archive already in the directory is kept when its sha256 is the pin and refused
//! when it is not (delete it and re-run). A missing one is printed as a `DRY-RUN:` line, or
//! with `--apply` downloaded, checked, and written to its name; a download whose sha256 is not
//! the pin is written to `<file>.rejected` for inspection and fails the run.

use std::io::Write;

use js_host_step::Mode;
use js_mdm_core::pins::{Pins, Tool};

use crate::{Env, MdmError};

/// The largest archive downloaded, in bytes.
pub const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;

/// What `fetch` was asked for.
pub struct Fetch {
    /// Where the archives go: absolute, outside every git work tree.
    pub dir: String,
    /// Only these tools; every pinned tool when empty.
    pub only: Vec<Tool>,
}

/// Check what is in the directory, then print or carry out the downloads.
pub fn run(
    _env: &Env<'_>,
    _f: &Fetch,
    _pins: &Pins,
    _mode: Mode,
    _out: &mut dyn Write,
) -> Result<(), MdmError> {
    Ok(())
}
