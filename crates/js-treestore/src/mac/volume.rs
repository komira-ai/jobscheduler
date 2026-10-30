//! The store volume checks, over the facts `statfs` gives.

use std::io;
use std::path::Path;

/// `MNT_NOEXEC`: no program on the volume can run.
const MNT_NOEXEC: u32 = 0x4;
/// `MNT_IGNORE_OWNERSHIP`: the volume takes no notice of file owners, so root-owned sealing
/// and the separation of users mean nothing on it.
const MNT_IGNORE_OWNERSHIP: u32 = 0x0020_0000;

/// What `statfs` says about a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeFacts {
    /// The file system type name (`f_fstypename`).
    pub fs_type: String,
    /// The mount flags (`f_flags`).
    pub flags: u32,
    /// The volume's identity (`f_fsid`).
    pub fsid: [i32; 2],
}

/// A volume is not fit to hold the store.
#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    /// The volume is not APFS.
    #[error("{what} volume is {fs_type}, not apfs")]
    NotApfs {
        /// Which volume.
        what: &'static str,
        /// What it is.
        fs_type: String,
    },
    /// The volume is mounted ignoring ownership.
    #[error("{what} volume is mounted ignoring ownership")]
    IgnoresOwnership {
        /// Which volume.
        what: &'static str,
    },
    /// The volume is mounted noexec.
    #[error("{what} volume is mounted noexec")]
    NoExec {
        /// Which volume.
        what: &'static str,
    },
    /// The store and the scratch root are not on one volume.
    #[error("the store and the scratch root are on different volumes")]
    DifferentVolumes,
    /// A volume could not be read.
    #[error("cannot read the {what} volume: {source}")]
    Read {
        /// Which volume.
        what: &'static str,
        /// The error.
        source: io::Error,
    },
}

/// Checks that the store's volume is APFS, mounted with ownership honoured and not noexec,
/// and that the scratch root is on the same volume.
///
/// # Errors
/// The first check that fails.
pub fn check_volumes(
    store: &Path,
    scratch: &Path,
    read: impl Fn(&Path) -> io::Result<VolumeFacts>,
) -> Result<(), VolumeError> {
    let on_store = facts(store, "store", &read)?;
    if on_store.fs_type != "apfs" {
        return Err(VolumeError::NotApfs {
            what: "store",
            fs_type: on_store.fs_type,
        });
    }
    if on_store.flags & MNT_IGNORE_OWNERSHIP != 0 {
        return Err(VolumeError::IgnoresOwnership { what: "store" });
    }
    if on_store.flags & MNT_NOEXEC != 0 {
        return Err(VolumeError::NoExec { what: "store" });
    }
    let on_scratch = facts(scratch, "scratch", &read)?;
    if on_scratch.fsid != on_store.fsid {
        return Err(VolumeError::DifferentVolumes);
    }
    Ok(())
}

fn facts(
    path: &Path,
    what: &'static str,
    read: &impl Fn(&Path) -> io::Result<VolumeFacts>,
) -> Result<VolumeFacts, VolumeError> {
    read(path).map_err(|source| VolumeError::Read { what, source })
}
