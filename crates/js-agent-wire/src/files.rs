//! Files a start delivers to the job: per-run secrets such as a presigned URL or a grant
//! token, which `JobSpec` has no environment for.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::check::{self, At};
use crate::error::{Rule, ValidationError};
use crate::limits;
use crate::refusal;

/// One file the agent writes for the job before its container is created. The job reads
/// it at `/run/js-files/<name>`; the directory is mounted read-only, so the file is
/// `0444` there.
///
/// The contents are secret. `Debug` prints the name and the length of the contents and
/// nothing else, so no log line or panic message that prints a start can leak them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFile {
    /// The file name: `[A-Za-z0-9._-]`, leading alphanumeric, at most
    /// [`limits::MAX_RUN_FILE_NAME_LEN`] bytes (the grammar of a mount root name), and
    /// not the name of another file of the same start.
    pub name: String,
    /// The contents, UTF-8 text such as a URL or a token: at most
    /// [`limits::MAX_RUN_FILE_LEN`] bytes, no NUL, and with the other files at most
    /// [`limits::MAX_RUN_FILES_TOTAL_BYTES`].
    pub contents: String,
}

impl fmt::Debug for RunFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunFile")
            .field("name", &self.name)
            .field("contents_len", &self.contents.len())
            .finish_non_exhaustive()
    }
}

/// Checks the run files of a start: the count, each name and size, no repeated name and
/// the total size. A fault reports field `files` and [`refusal::INVALID_FILE`], and never
/// repeats a name or any contents.
///
/// `JobSpec::validate` calls it; the run API calls it alone so that a bad file is
/// answered before the job is built.
pub fn validate_files(files: &[RunFile]) -> Result<(), ValidationError> {
    let at = At::new("files", refusal::INVALID_FILE);
    check::count(at, files.len(), limits::MAX_RUN_FILES)?;
    let mut total = 0usize;
    for file in files {
        check::token(
            at,
            &file.name,
            limits::MAX_RUN_FILE_NAME_LEN,
            check::ident_byte,
        )?;
        if file.contents.len() > limits::MAX_RUN_FILE_LEN {
            return Err(at.fail(Rule::TooLong));
        }
        if file.contents.contains('\0') {
            return Err(at.fail(Rule::BadChars));
        }
        total += file.contents.len();
    }
    if total > limits::MAX_RUN_FILES_TOTAL_BYTES {
        return Err(at.fail(Rule::TooLong));
    }
    // Compared in lower case: two names that differ only in case are one file on a file
    // system that folds case (a Mac volume), and a reader should not have to tell them apart.
    check::distinct(at, files.iter().map(|file| file.name.to_ascii_lowercase()))
}
