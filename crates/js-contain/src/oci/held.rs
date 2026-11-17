//! A container made ahead of its job (a warm sandbox, [`Oci::create_held`]): its init waits
//! on the lease's named pipe, and [`hand_job`] writes the job to it once the caller has bound
//! the job's directories into the container.
//!
//! The job is the program, its arguments, its working directory and its whole environment,
//! as [`action_config`](super::spec::action_config) would have put them in `process`: the
//! working directory is the exec root joined with the spec's own, and the environment is the
//! spec's with `HOSTNAME` and `HOME` defaulted ([`environment`]). Its form, which the init
//! (`init/init.rs`, `parse_job`) reads: the field `js-job/1`, the working directory, the
//! count of arguments and each argument, the count of variables and each `NAME=VALUE`, every
//! field ended by a NUL byte.
//!
//! [`Oci::create_held`]: super::Oci::create_held

use std::io::Write as _;
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags};

use super::spec::{JOB_PIPE, environment};
use super::{JobError, at};
use crate::container::ContainerSpec;

/// The first field of a job, which names its form.
const FORM: &str = "js-job/1";

/// Why a job could not be handed to its held container.
#[derive(Debug, thiserror::Error)]
pub enum HandError {
    /// A field of the job holds a NUL byte, which ends a field: the job would mean another.
    #[error("the job's {0} holds a NUL byte")]
    Nul(&'static str),
    /// The job names no program.
    #[error("the job names no program")]
    NoProgram,
    /// No init waits on the pipe: the container is not up yet, or has ended.
    #[error("no init waits for a job on {0}")]
    NotWaiting(PathBuf),
    /// The pipe could not be opened or written.
    #[error("{path}: {source}")]
    Io {
        /// The pipe.
        path: PathBuf,
        /// The system's reason.
        #[source]
        source: std::io::Error,
    },
}

/// The named pipe of the lease in `lease` that its held init reads the job from.
pub fn job_pipe(lease: &Path) -> PathBuf {
    lease.join(JOB_PIPE)
}

/// Makes the lease's pipe (`0600`, the daemon's), replacing one a failed make left.
pub(super) fn make_pipe(lease: &Path) -> Result<(), JobError> {
    let pipe = job_pipe(lease);
    match std::fs::remove_file(&pipe) {
        Err(why) if why.kind() != std::io::ErrorKind::NotFound => return at(&pipe, Err(why)),
        _ => {}
    }
    let mode = Mode::from_raw_mode(0o600);
    let made = rustix::fs::mknodat(rustix::fs::CWD, &pipe, FileType::Fifo, mode, 0);
    at(&pipe, made.map_err(std::io::Error::from))?;
    // The umask may have taken bits: the mode is set again by the path, in a directory that
    // is the daemon's.
    at(&pipe, rustix::fs::chmod(&pipe, mode).map_err(std::io::Error::from))
}

/// The bytes of the job of `spec` ([`FORM`] and its fields, each ended by a NUL byte).
///
/// # Errors
/// [`HandError::NoProgram`], or [`HandError::Nul`] naming the field that holds a NUL byte.
pub fn job_bytes(spec: &ContainerSpec) -> Result<Vec<u8>, HandError> {
    if spec.argv.is_empty() {
        return Err(HandError::NoProgram);
    }
    let cwd = match spec.working_directory.is_empty() {
        true => spec.exec_root.clone(),
        false => format!("{}/{}", spec.exec_root, spec.working_directory),
    };
    let env = environment(&spec.env);
    let mut fields: Vec<(&'static str, &str)> = vec![("form", FORM), ("working directory", &cwd)];
    let (argc, envc) = (spec.argv.len().to_string(), env.len().to_string());
    fields.push(("argument count", &argc));
    fields.extend(spec.argv.iter().map(|arg| ("arguments", arg.as_str())));
    fields.push(("variable count", &envc));
    fields.extend(env.iter().map(|var| ("environment", var.as_str())));
    let mut bytes = Vec::new();
    for (what, field) in fields {
        if field.contains('\0') {
            return Err(HandError::Nul(what));
        }
        bytes.extend_from_slice(field.as_bytes());
        bytes.push(0);
    }
    Ok(bytes)
}

/// Hands the job of `spec` (its argv, environment and working directory) to the held init
/// of its container, which starts it at once: the pipe in `spec.dir` is opened without
/// waiting (an init that is not waiting on it is [`HandError::NotWaiting`], and nothing is
/// written), and the job is written whole. Called once, after the job's directories are
/// bound into the container: from then on the job runs.
///
/// # Errors
/// The job cannot be written in its form ([`job_bytes`]), no init waits, or the pipe could
/// not be opened or written.
pub fn hand_job(spec: &ContainerSpec) -> Result<(), HandError> {
    let bytes = job_bytes(spec)?;
    let path = job_pipe(&spec.dir);
    let io = |source: rustix::io::Errno| HandError::Io {
        path: path.clone(),
        source: source.into(),
    };
    let flags = OFlags::WRONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let pipe = match rustix::fs::open(&path, flags, Mode::empty()) {
        Err(rustix::io::Errno::NXIO) => return Err(HandError::NotWaiting(path.clone())),
        opened => opened.map_err(io)?,
    };
    // Written whole, waiting for the reader as it reads: a job longer than the pipe holds
    // is not cut short.
    rustix::fs::fcntl_setfl(&pipe, OFlags::empty()).map_err(io)?;
    std::fs::File::from(pipe)
        .write_all(&bytes)
        .map_err(|source| HandError::Io {
            path: path.clone(),
            source,
        })
}
