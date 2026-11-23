//! The spec of one supervised start: what the agent writes to `<record_dir>/spec.json` and the
//! supervisor reads once, then removes ([`read_spec`]).

use std::fs::OpenOptions;
use std::io::{self, Read as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// What the supervisor needs to start one native job. The agent writes it as JSON, root's and
/// readable by root alone; nothing of it is on the supervisor's command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuperviseSpec {
    /// The slot account's user id, which the supervisor drops to.
    pub uid: u32,
    /// The slot account's group id, its only group after the drop.
    pub gid: u32,
    /// Whether the supervisor opens a new audit session, with the slot uid as its audit user:
    /// true from the agent, false only in tests (it needs root).
    pub new_session: bool,
    /// The Seatbelt profile's text (`js_mac_pool::seatbelt::Profile::text`).
    pub profile: String,
    /// The profile's parameters, name and value, in order
    /// (`js_mac_pool::seatbelt::Profile::params`).
    pub params: Vec<(String, String)>,
    /// The job's program: an absolute path, already resolved by the agent.
    pub program: String,
    /// The program's arguments, after the program itself.
    pub args: Vec<String>,
    /// The largest file the job may write, in bytes (`RLIMIT_FSIZE`, soft and hard).
    pub max_file_bytes: u64,
    /// The most processes the slot uid may have (`RLIMIT_NPROC`, soft and hard).
    pub max_processes: u32,
    /// The directory the records are written in: absolute, made by the agent, root's, mode
    /// 0700, in the run's scratch.
    pub record_dir: String,
}

/// Why a spec was refused. The supervisor then exits with
/// [`super::status::USAGE`] and writes nothing.
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    /// The file could not be opened (a symbolic link is refused here: it is opened without
    /// following one) or its status could not be read.
    #[error("opening the spec: {0}")]
    Open(#[source] io::Error),
    /// The path names something that is not a regular file.
    #[error("the spec is not a regular file")]
    NotRegular,
    /// The file could not be removed, so it would stay readable after the start.
    #[error("removing the spec: {0}")]
    Unlink(#[source] io::Error),
    /// The file belongs to another user than the one reading it.
    #[error("the spec belongs to uid {owner}, not to uid {caller}")]
    Owner {
        /// The file's owner.
        owner: u32,
        /// The user reading it.
        caller: u32,
    },
    /// The file's group or others hold a permission bit: someone else could have changed it.
    #[error("the spec has mode {0:o}; only its owner may hold permissions")]
    Mode(u32),
    /// The file could not be read, or it is not UTF-8.
    #[error("reading the spec: {0}")]
    Read(#[source] io::Error),
    /// The text is not a spec.
    #[error("the spec is not a supervisor spec: {0}")]
    Json(String),
    /// A string holds a NUL, which no C call can take.
    #[error("the spec holds a NUL")]
    Nul,
    /// The program is not an absolute path.
    #[error("the program {0:?} is not an absolute path")]
    RelativeProgram(String),
    /// The record directory is not an absolute path.
    #[error("the record directory {0:?} is not an absolute path")]
    RelativeRecordDir(String),
}

/// Decodes a spec: JSON with no unknown field, no NUL in any string, an absolute program and
/// an absolute record directory.
///
/// # Errors
/// [`SpecError::Json`], [`SpecError::Nul`], [`SpecError::RelativeProgram`] or
/// [`SpecError::RelativeRecordDir`].
pub fn parse(text: &str) -> Result<SuperviseSpec, SpecError> {
    let spec: SuperviseSpec =
        serde_json::from_str(text).map_err(|why| SpecError::Json(why.to_string()))?;
    if has_nul(&spec) {
        return Err(SpecError::Nul);
    }
    if !Path::new(&spec.program).is_absolute() {
        return Err(SpecError::RelativeProgram(spec.program));
    }
    if !Path::new(&spec.record_dir).is_absolute() {
        return Err(SpecError::RelativeRecordDir(spec.record_dir));
    }
    Ok(spec)
}

/// Whether a string of `spec` holds a NUL.
fn has_nul(spec: &SuperviseSpec) -> bool {
    [&spec.profile, &spec.program, &spec.record_dir]
        .into_iter()
        .chain(&spec.args)
        .chain(spec.params.iter().flat_map(|(name, value)| [name, value]))
        .any(|text| text.contains('\0'))
}

/// Reads the spec at `path` once: opens it without following a symbolic link (and without
/// blocking, should it be a pipe), refuses anything but a regular file, removes it, then
/// refuses it unless it belongs to the caller's effective uid with no permission bit for its
/// group or others, reads it and parses it ([`parse`]).
///
/// The file is removed before anything else of it is judged, so a spec that is refused, or
/// that does not parse, is gone as well: no profile text or parameter list stays on disk for
/// the job's user to read, and a spec is never used twice.
///
/// # Errors
/// [`SpecError::Open`], [`SpecError::NotRegular`], [`SpecError::Unlink`],
/// [`SpecError::Owner`], [`SpecError::Mode`], [`SpecError::Read`], or what [`parse`] refuses.
pub fn read_spec(path: &Path) -> Result<SuperviseSpec, SpecError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(SpecError::Open)?;
    let status = file.metadata().map_err(SpecError::Open)?;
    if !status.file_type().is_file() {
        return Err(SpecError::NotRegular);
    }
    std::fs::remove_file(path).map_err(SpecError::Unlink)?;
    let caller = rustix::process::geteuid().as_raw();
    if let Some(why) = loose(status.uid(), caller, status.mode()) {
        return Err(why);
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(SpecError::Read)?;
    parse(&text)
}

/// Why a file of `owner` with `mode`, read by `caller`, cannot be trusted: another owner, or a
/// permission bit of its group or others. `None` when it can.
fn loose(owner: u32, caller: u32, mode: u32) -> Option<SpecError> {
    if owner != caller {
        return Some(SpecError::Owner { owner, caller });
    }
    (mode & 0o077 != 0).then_some(SpecError::Mode(mode & 0o7777))
}

#[cfg(test)]
#[path = "spec_tests.rs"]
pub(crate) mod tests;
