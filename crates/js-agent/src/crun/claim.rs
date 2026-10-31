//! One crun agent for each host id, and for each scratch root, on a host.
//!
//! The lease names, the sidecar files and the start-up sweep of a [`CrunRuntime`](super::CrunRuntime)
//! derive from the agent's host id, and the sweep removes every entry of the scratch root that
//! is not one of the agent's runs. Two agents that share a host id (the default id comes from
//! the machine, so two agents of one machine share it), or a scratch root and the store beside
//! it, would remove each other's leases and run directories. The agent takes both claims
//! before it registers (and so before the sweep) and holds them for as long as it runs.
//!
//! Both claim files live in one directory, the one above the scratch root, so agents that
//! share a host id must share that directory too: two agents whose scratch roots have
//! different parents do not see each other's host-id claim.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Seek as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::facts::FileReader;

/// Why a claim could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    /// Another agent holds the claim, or it cannot be locked.
    #[error(
        "{what}: another crun agent of this host holds it, or it cannot be locked (holder: {holder}): two agents that share it would remove each other's leases; give \
         each agent its own"
    )]
    Held {
        /// What is claimed, in words that name the flag.
        what: String,
        /// What the holder wrote in the file: its process id, or `not yet written` when the
        /// holder has the lock and has not written yet.
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

/// A claim, held for as long as the value lives. The lock is dropped by the system when the
/// process ends, so a restarted agent finds it free.
#[derive(Debug)]
pub struct HostClaim {
    _held: File,
}

/// The name of the claim file of the `key` of a `kind`: a hash, so any key makes a plain file
/// name.
fn file_name(kind: &str, key: &[u8]) -> String {
    let hash = Sha256::digest(key);
    format!(".js-agent-{kind}-{}.lock", hex::encode(&hash[..16]))
}

impl HostClaim {
    /// Takes the claim of `host_id` in `dir`, a directory every crun agent of the host uses.
    ///
    /// # Errors
    /// [`ClaimError::Held`] when another process holds it; [`ClaimError::Io`] otherwise.
    pub fn take(dir: &Path, host_id: &str) -> Result<Self, ClaimError> {
        Self::keyed(
            dir,
            "host",
            host_id.as_bytes(),
            format!("--host-id {host_id}"),
        )
    }

    /// Takes the claim of the scratch root `root` in `dir`, which is the directory above it:
    /// one agent for each scratch root and the store beside it. The claim is keyed on the
    /// device and inode of the directory (read from a descriptor opened without following a
    /// link), not on its path, so every path to the directory, a link or a bind mount
    /// included, names the same claim.
    ///
    /// # Errors
    /// As [`HostClaim::take`]; [`ClaimError::Io`] also when `root` cannot be opened as a
    /// directory.
    pub fn scratch_root(dir: &Path, root: &Path) -> Result<Self, ClaimError> {
        let what = format!("--scratch-root {}", root.display());
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(root)
            .and_then(|handle| handle.metadata());
        let meta = opened.map_err(|why| ClaimError::Io {
            path: root.to_owned(),
            why,
        })?;
        let key = format!("{}:{}", meta.dev(), meta.ino());
        Self::keyed(dir, "scratch", key.as_bytes(), what)
    }

    fn keyed(dir: &Path, kind: &str, key: &[u8], what: String) -> Result<Self, ClaimError> {
        let path = dir.join(file_name(kind, key));
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
            let holder = match said.trim() {
                "" => "not yet written".to_owned(),
                written => written.to_owned(),
            };
            return Err(ClaimError::Held { what, holder });
        }
        file.set_len(0).map_err(io)?;
        file.rewind().map_err(io)?;
        file.write_all(std::process::id().to_string().as_bytes())
            .map_err(io)?;
        Ok(Self { _held: file })
    }
}

/// Whether `a` and `b` are on one file system, as `files` reads their devices.
///
/// # Errors
/// A device cannot be read; the text says which.
pub fn same_filesystem(files: &dyn FileReader, a: &Path, b: &Path) -> Result<bool, String> {
    Ok(files.device(a)? == files.device(b)?)
}

/// Checks that every directory above the scratch root `root`, up to `/`, is owned by root or
/// by `euid` (the agent's user) and that no group or other can write it: whoever can write
/// an ancestor can swap the scratch root, the store or a claim file for a link. The path is
/// resolved first, so the directories checked are the ones the kernel walks.
///
/// # Errors
/// The text names the ancestor at fault.
pub fn require_trusted_ancestors(root: &Path, euid: u32) -> Result<(), String> {
    let canonical = fs::canonicalize(root).map_err(|why| format!("{}: {why}", root.display()))?;
    for ancestor in canonical.ancestors().skip(1) {
        let meta =
            fs::metadata(ancestor).map_err(|why| format!("{}: {why}", ancestor.display()))?;
        let trusted = (meta.uid() == 0 || meta.uid() == euid) && meta.mode() & 0o022 == 0;
        if !trusted {
            return Err(format!(
                "{}: a directory above the scratch root must be owned by root or by the user                  the agent runs as, and no group or other may write it",
                ancestor.display()
            ));
        }
    }
    Ok(())
}

/// Checks the directory `parent` (above the scratch root) and makes and checks the store
/// `store` in it, before a claim file or the store's contents go there: each must be a
/// directory, not a link, owned by the user the agent runs as, that no group or other can
/// write.
///
/// # Errors
/// The text names the path at fault.
pub fn prepare_store(parent: &Path, store: &Path) -> Result<(), String> {
    let me = rustix::process::geteuid().as_raw();
    let verify = |dir: &Path| -> Result<(), String> {
        let meta = fs::symlink_metadata(dir).map_err(|why| format!("{}: {why}", dir.display()))?;
        match meta.is_dir() && meta.uid() == me && meta.mode() & 0o022 == 0 {
            true => Ok(()),
            false => Err(format!(
                "{}: must be a directory (not a link) owned by the user the agent runs as that no \
                 group or other can write: the image store and the claim files are kept there",
                dir.display()
            )),
        }
    };
    verify(parent)?;
    // A store that is there already (or a link, which `verify` refuses) is left as it is.
    let _ = fs::DirBuilder::new().mode(0o755).create(store);
    verify(store)
}
