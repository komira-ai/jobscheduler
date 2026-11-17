//! The gathering of a job's declared outputs when the job has ended: each path opened below the
//! job's working directory, from the directory down, following no symbolic link at any step,
//! and each output file kept open, so the upload reads it after the job's files are removed
//! (`docs/design/job-outputs.md`, section 3).
//!
//! A name is looked at without following it (`AT_SYMLINK_NOFOLLOW`) before it is opened, and
//! opened with `O_NOFOLLOW` (and, for a file, `O_NONBLOCK`): a link, a device, a socket or a
//! pipe is never opened, and what was opened is held to the kind that was looked at
//! (`fstat`), so a name swapped between the two is refused, not read. A regular file with
//! more than one name is refused too (a job may give a second name to a file it cannot read,
//! and the agent reads as root).

use std::ffi::CString;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::Path;

use js_logstore::outputs::{OutputFailure, OutputPath, check_declared};
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, fstat, openat, statat};
use rustix::io::Errno;

/// The flags a directory is opened with.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// The flags an output file is opened with.
const FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// A path that is not one ([`check_declared`] refused the declaration).
pub(crate) const PATH: &str = "path";
/// A link, a device, a socket, a pipe, a file with more than one name, or a name on the way
/// that is not a directory.
pub(crate) const NOT_A_FILE: &str = "not-a-file";
/// A file over the most one file may hold.
pub(crate) const TOO_LARGE: &str = "too-large";
/// Past the most files or bytes the outputs may hold together, or deeper than a tree may go.
pub(crate) const OVER_LIMIT: &str = "over-limit";
/// A name in a directory that is not an output path's segment.
pub(crate) const NAME: &str = "name";
/// A name that could not be looked at, opened or listed.
pub(crate) const READ: &str = "read";

/// What a gathering may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Caps {
    /// The most bytes of one file.
    pub(crate) file_bytes: u64,
    /// The most bytes of all the files.
    pub(crate) total_bytes: u64,
    /// The most files.
    pub(crate) files: usize,
    /// The most levels of directories a declared directory's tree may hold, the declared
    /// directory the first: a directory past them is refused, the files above it gathered.
    pub(crate) depth: usize,
}

/// One output file: its path and the file, open.
#[derive(Debug)]
pub(crate) struct Gotten {
    /// Its path below the working directory.
    pub(crate) path: OutputPath,
    /// The file, open to read.
    pub(crate) file: File,
}

/// What a gathering found: the files in the order of the declaration (a directory's in the
/// order of their names), the declared paths the job did not make, and the paths refused.
#[derive(Debug, Default)]
pub(crate) struct Gathered {
    /// The files.
    pub(crate) files: Vec<Gotten>,
    /// The declared paths not there.
    pub(crate) missing: Vec<String>,
    /// The paths refused, and why.
    pub(crate) failed: Vec<OutputFailure>,
}

/// What may still be taken.
struct Left {
    caps: Caps,
    bytes: u64,
    files: usize,
}

/// Gathers the outputs `declared` below the directory `root` within `caps`.
pub(crate) fn gather(root: &Path, declared: &[String], caps: Caps) -> Gathered {
    let mut found = Gathered::default();
    let paths = match check_declared(declared) {
        Ok(paths) => paths,
        Err((at, why)) => {
            tracing::warn!(at, "job outputs: the declaration is refused: {why}");
            found.failed = declared.iter().map(|path| failure(path, PATH)).collect();
            return found;
        }
    };
    let top = match openat(CWD, root, DIR_FLAGS, Mode::empty()) {
        Ok(top) => top,
        Err(Errno::NOENT) => {
            found.missing = paths.iter().map(ToString::to_string).collect();
            return found;
        }
        Err(_) => {
            found.failed = paths
                .iter()
                .map(|path| failure(path.as_str(), READ))
                .collect();
            return found;
        }
    };
    let mut left = Left {
        caps,
        bytes: caps.total_bytes,
        files: caps.files,
    };
    for path in paths {
        one(&top, path, &mut left, &mut found);
    }
    found
}

/// Gathers the declared output `path`.
fn one(top: &OwnedFd, path: OutputPath, left: &mut Left, found: &mut Gathered) {
    let text = path.to_string();
    let names: Vec<&str> = text.split('/').collect();
    let (last, above) = names.split_last().unwrap_or((&"", &[]));
    let mut dir = None;
    for name in above {
        let at = dir.as_ref().unwrap_or(top);
        match open_dir(at, name) {
            Ok(Some(next)) => dir = Some(next),
            Ok(None) => return found.missing.push(text),
            Err(why) => return found.failed.push(failure(&text, why)),
        }
    }
    let at = dir.as_ref().unwrap_or(top);
    match entry(at, last, path, left, found, 0) {
        Ok(true) => {}
        Ok(false) => found.missing.push(text),
        Err((at, why)) => found.failed.push(failure(&at, why)),
    }
}

/// Takes the entry `name` of `dir`, at `path`, `depth` levels below a declared directory: a
/// file is gathered, a directory walked. `false` when there is no such entry.
fn entry(
    dir: &OwnedFd,
    name: &str,
    path: OutputPath,
    left: &mut Left,
    found: &mut Gathered,
    depth: usize,
) -> Result<bool, (String, &'static str)> {
    let fail = |why| (path.to_string(), why);
    // A segment holds no NUL.
    let c_name = CString::new(name).unwrap_or_default();
    let kind = match statat(dir, c_name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => FileType::from_raw_mode(stat.st_mode),
        Err(Errno::NOENT) => return Ok(false),
        Err(_) => return Err(fail(READ)),
    };
    match kind {
        FileType::RegularFile => {
            let file = open_file(dir, &c_name, left).map_err(fail)?;
            found.files.push(Gotten { path, file });
        }
        FileType::Directory if depth == left.caps.depth => return Err(fail(OVER_LIMIT)),
        FileType::Directory => {
            let (sub, names) = list(dir, &c_name).map_err(|()| fail(READ))?;
            walk(&sub, &names, &path, left, found, depth + 1);
        }
        _ => return Err(fail(NOT_A_FILE)),
    }
    Ok(true)
}

/// The directory `name` of `dir`, opened following no link, and its names, sorted, without
/// `.` and `..`.
fn list(dir: &OwnedFd, name: &CString) -> Result<(OwnedFd, Vec<Vec<u8>>), ()> {
    let opened = openat(dir, name.as_c_str(), DIR_FLAGS, Mode::empty())
        .and_then(|sub| Dir::read_from(&sub).map(|listed| (sub, listed)));
    let (sub, listed) = opened.map_err(|_| ())?;
    let mut names: Vec<Vec<u8>> = listed
        .flatten()
        .map(|entry| entry.file_name().to_bytes().to_vec())
        .filter(|name| !matches!(name.as_slice(), b"." | b".."))
        .collect();
    names.sort();
    Ok((sub, names))
}

/// Gathers each of the entries `names` of the directory `dir`, at `path`, in order.
fn walk(
    dir: &OwnedFd,
    names: &[Vec<u8>],
    path: &OutputPath,
    left: &mut Left,
    found: &mut Gathered,
    depth: usize,
) {
    for name in names {
        let below = std::str::from_utf8(name)
            .ok()
            .and_then(|name| path.join(name).ok().map(|below| (name, below)));
        let Some((name, below)) = below else {
            let shown = format!("{path}/{}", shown(name));
            found.failed.push(failure(&shown, NAME));
            continue;
        };
        if let Err((at, why)) = entry(dir, name, below, left, found, depth) {
            found.failed.push(failure(&at, why));
        }
    }
}

/// Opens the directory `name` of `dir` on the way to an output, following no link; `None`
/// when it is not there.
fn open_dir(dir: &OwnedFd, name: &str) -> Result<Option<OwnedFd>, &'static str> {
    // A segment holds no NUL.
    let c_name = CString::new(name).unwrap_or_default();
    match openat(dir, c_name.as_c_str(), DIR_FLAGS, Mode::empty()) {
        Ok(opened) => Ok(Some(opened)),
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => Err(NOT_A_FILE),
        Err(_) => Err(READ),
    }
}

/// Opens the regular file `name` of `dir` and takes its length from `left`.
fn open_file(dir: &OwnedFd, name: &CString, left: &mut Left) -> Result<File, &'static str> {
    let opened = openat(dir, name.as_c_str(), FILE_FLAGS, Mode::empty())
        .and_then(|opened| fstat(&opened).map(|stat| (opened, stat)));
    let (opened, stat) = opened.map_err(|_| READ)?;
    // Held to the kind looked at: a name swapped since is refused, as is a second name.
    match (FileType::from_raw_mode(stat.st_mode), stat.st_nlink) {
        (FileType::RegularFile, 1) => {}
        _ => return Err(NOT_A_FILE),
    }
    let len = u64::try_from(stat.st_size).unwrap_or(u64::MAX);
    if len > left.caps.file_bytes {
        return Err(TOO_LARGE);
    }
    if left.files == 0 || len > left.bytes {
        return Err(OVER_LIMIT);
    }
    left.files -= 1;
    left.bytes -= len;
    Ok(File::from(opened))
}

/// A failure of `path` for `reason`.
fn failure(path: &str, reason: &str) -> OutputFailure {
    OutputFailure {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

/// A name as a failure shows it: each byte outside `[A-Za-z0-9._-]` as `?`.
fn shown(name: &[u8]) -> String {
    let keep = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
    name.iter()
        .map(|&b| if keep(b) { char::from(b) } else { '?' })
        .collect()
}
