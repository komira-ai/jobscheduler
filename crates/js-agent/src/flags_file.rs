//! `--flags-file <PATH>`: flags kept in a file, so a unit or a launchd job that cannot name
//! the host's server and pool (a package is public and the same on every host) still passes
//! them as flags.
//!
//! [`expand`] runs before clap reads the arguments. It replaces the pair `--flags-file PATH`
//! (or the one word `--flags-file=PATH`) by the words of the file, in place, so a word given
//! after the pair on the command line is read after the file's. The file has one argument
//! word per line, such as `--server=https://leases.example:7300`; surrounding whitespace is
//! trimmed, and empty lines and lines that start with `#` are skipped. [`words`] is that line
//! rule alone, for a caller that reads the file itself (the update hook).
//!
//! The file is trusted like a command line: it must be a regular file (a link is followed),
//! owned by the user running the agent or by root, not writable by group or others, and at
//! most [`MAX_BYTES`] of UTF-8. Every line must start with `--`, none may be
//! [`FLAG`] again, and the flag may be given only once.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The flag that names the file.
pub const FLAG: &str = "--flags-file";

/// The most bytes a flags file may hold.
pub const MAX_BYTES: u64 = 64 * 1024;

/// Why a flags file was refused. The text names the file and says why; it never holds a
/// byte of a line that is wrong beyond its number.
#[derive(Debug, thiserror::Error)]
pub enum FlagsFileError {
    /// `--flags-file` was given more than once.
    #[error("{FLAG}: given more than once")]
    Repeated,
    /// `--flags-file` was the last word, with no path.
    #[error("{FLAG}: needs a path")]
    MissingPath,
    /// The file cannot be opened or read.
    #[error("{FLAG} {path}: cannot be read: {source}")]
    Unreadable {
        /// The file.
        path: PathBuf,
        /// What the system said.
        source: io::Error,
    },
    /// The file, after following links, is not a regular file.
    #[error("{FLAG} {path}: not a regular file")]
    NotRegular {
        /// The file.
        path: PathBuf,
    },
    /// Group or others can write the file.
    #[error("{FLAG} {path}: writable by group or others (mode {mode:04o})")]
    Writable {
        /// The file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The file belongs to neither the user running the agent nor root.
    #[error("{FLAG} {path}: owned by user {uid}, not by the user running the agent or root")]
    Owner {
        /// The file.
        path: PathBuf,
        /// Its owner.
        uid: u32,
    },
    /// The file is larger than [`MAX_BYTES`].
    #[error("{FLAG} {path}: larger than {MAX_BYTES} bytes")]
    TooLarge {
        /// The file.
        path: PathBuf,
    },
    /// The file is not UTF-8.
    #[error("{FLAG} {path}: not UTF-8 text")]
    NotUtf8 {
        /// The file.
        path: PathBuf,
    },
    /// A line of the file is `--flags-file`: files do not include files.
    #[error("line {line}: {FLAG} may not appear in a flags file")]
    Nested {
        /// The 1-based line.
        line: usize,
    },
    /// A line of the file does not start with `--`.
    #[error("line {line}: not a flag (every line must start with --)")]
    NotAFlag {
        /// The 1-based line.
        line: usize,
    },
    /// A line of the file is wrong, with the file it is in.
    #[error("{FLAG} {path}: {source}")]
    Line {
        /// The file.
        path: PathBuf,
        /// The line rule that failed ([`Nested`](Self::Nested) or [`NotAFlag`](Self::NotAFlag)).
        source: Box<FlagsFileError>,
    },
}

/// What is known of a flags file: the facts [`expand_with`] judges it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFacts {
    /// Whether it is a regular file after following links.
    pub regular: bool,
    /// Its permission bits.
    pub mode: u32,
    /// Its owner.
    pub uid: u32,
    /// Its content, at most [`MAX_BYTES`] and one byte more when the file is larger; empty
    /// when it is not a regular file.
    pub bytes: Vec<u8>,
}

/// The words of a flags file's text, one per line.
///
/// # Errors
///
/// [`FlagsFileError::Nested`] for a line that names `--flags-file` (alone or with `=`), and
/// [`FlagsFileError::NotAFlag`] for a line that does not start with `--` (or is only `--`).
pub fn words(text: &str) -> Result<Vec<String>, FlagsFileError> {
    let mut found = Vec::new();
    for (at, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line_no = at + 1;
        if line
            .strip_prefix(FLAG)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
        {
            return Err(FlagsFileError::Nested { line: line_no });
        }
        if line.len() <= 2 || !line.starts_with("--") {
            return Err(FlagsFileError::NotAFlag { line: line_no });
        }
        found.push(line.to_owned());
    }
    Ok(found)
}

/// Reads the facts of the file at `path` from the system: the file is opened (a link is
/// followed, a pipe is not waited on) and looked at through the descriptor.
///
/// # Errors
///
/// The file cannot be opened or read.
pub fn read_system(path: &Path) -> io::Result<FileFacts> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    let mut bytes = Vec::new();
    if meta.is_file() {
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    }
    Ok(FileFacts {
        regular: meta.is_file(),
        mode: meta.mode() & 0o7777,
        uid: meta.uid(),
        bytes,
    })
}

/// [`expand_with`] for the user running the agent and the system's files.
///
/// # Errors
///
/// See [`expand_with`].
pub fn expand(args: Vec<OsString>) -> Result<Vec<OsString>, FlagsFileError> {
    expand_with(args, rustix::process::geteuid().as_raw(), &read_system)
}

/// `args` (the program name first) with `--flags-file PATH` replaced by the file's words.
/// `euid` is the user the file may belong to besides root; `read` gives the facts of a file.
///
/// # Errors
///
/// A [`FlagsFileError`]: the flag twice or without a path, a file that cannot be read, is
/// not a regular file, is writable by group or others, belongs to another user, is too
/// large or not UTF-8, or a line that breaks the rule of [`words`].
pub fn expand_with(
    args: Vec<OsString>,
    euid: u32,
    read: &dyn Fn(&Path) -> io::Result<FileFacts>,
) -> Result<Vec<OsString>, FlagsFileError> {
    let mut out = Vec::with_capacity(args.len());
    let mut given = false;
    let mut rest = args.into_iter();
    out.extend(rest.next());
    while let Some(word) = rest.next() {
        let path = if word.as_bytes() == FLAG.as_bytes() {
            rest.next().ok_or(FlagsFileError::MissingPath)?
        } else if let Some(value) = word
            .as_bytes()
            .strip_prefix(FLAG.as_bytes())
            .and_then(|tail| tail.strip_prefix(b"="))
        {
            OsString::from_vec(value.to_vec())
        } else {
            out.push(word);
            continue;
        };
        if std::mem::replace(&mut given, true) {
            return Err(FlagsFileError::Repeated);
        }
        out.extend(
            words_of(Path::new(&path), euid, read)?
                .into_iter()
                .map(OsString::from),
        );
    }
    Ok(out)
}

/// The words of the file at `path`, after the checks of [`expand_with`].
fn words_of(
    path: &Path,
    euid: u32,
    read: &dyn Fn(&Path) -> io::Result<FileFacts>,
) -> Result<Vec<String>, FlagsFileError> {
    let owned = path.to_path_buf();
    let facts = read(path).map_err(|source| FlagsFileError::Unreadable {
        path: owned.clone(),
        source,
    })?;
    if !facts.regular {
        return Err(FlagsFileError::NotRegular { path: owned });
    }
    if facts.mode & 0o022 != 0 {
        return Err(FlagsFileError::Writable {
            path: owned,
            mode: facts.mode,
        });
    }
    if facts.uid != euid && facts.uid != 0 {
        return Err(FlagsFileError::Owner {
            path: owned,
            uid: facts.uid,
        });
    }
    if facts.bytes.len() as u64 > MAX_BYTES {
        return Err(FlagsFileError::TooLarge { path: owned });
    }
    let text = std::str::from_utf8(&facts.bytes).map_err(|_| FlagsFileError::NotUtf8 {
        path: owned.clone(),
    })?;
    words(text).map_err(|source| FlagsFileError::Line {
        path: owned,
        source: Box::new(source),
    })
}
