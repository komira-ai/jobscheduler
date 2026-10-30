//! One crun agent for each host id on a host.
//!
//! The lease names, the sidecar files and the start-up sweep of a [`CrunRuntime`](super::CrunRuntime)
//! derive from the agent's host id, so two agents that share a host id (the default id comes
//! from the machine, so two agents of one machine share it) and a store would remove each
//! other's leases. The agent takes this claim before it registers and holds it for as long as
//! it runs; a second agent with the same id is refused.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

/// Why a claim could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    /// Another agent holds the claim, or it cannot be locked.
    #[error(
        "--host-id {id}: another crun agent of this host holds it, or it cannot be locked \
         (the holder wrote {holder:?}): two agents that share a host id would remove each \
         other's leases; give each its own --host-id"
    )]
    Held {
        /// The host id.
        id: String,
        /// What the holder wrote in the file: its process id.
        holder: String,
    },
    /// The claim file could not be made, locked or written.
    #[error("{}: {why}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The reason.
        why: std::io::Error,
    },
}

/// The claim of a host id, held for as long as the value lives. The lock is dropped by the
/// system when the process ends, so a restarted agent finds it free.
#[derive(Debug)]
pub struct HostClaim {
    _held: File,
}

/// The name of the claim file of `host_id`: a hash, so any id makes a plain file name.
fn file_name(host_id: &str) -> String {
    let hash = Sha256::digest(host_id.as_bytes());
    format!(".js-agent-{}.lock", hex::encode(&hash[..16]))
}

impl HostClaim {
    /// Takes the claim of `host_id` in `dir`, a directory every crun agent of the host uses.
    ///
    /// # Errors
    /// [`ClaimError::Held`] when another process holds it; [`ClaimError::Io`] otherwise.
    pub fn take(dir: &Path, host_id: &str) -> Result<Self, ClaimError> {
        let path = dir.join(file_name(host_id));
        let io = |why| ClaimError::Io {
            path: path.clone(),
            why,
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(io)?;
        if file.try_lock().is_err() {
            let mut said = String::new();
            let _ = file.read_to_string(&mut said);
            return Err(ClaimError::Held {
                id: host_id.to_owned(),
                holder: said.trim().to_owned(),
            });
        }
        file.set_len(0).map_err(io)?;
        file.rewind().map_err(io)?;
        file.write_all(std::process::id().to_string().as_bytes())
            .map_err(io)?;
        Ok(Self { _held: file })
    }
}
